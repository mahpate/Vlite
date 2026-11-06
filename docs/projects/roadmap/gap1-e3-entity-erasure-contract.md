STATUS: DRAFT, paused 2026-11-06; unreviewed; NOT ratifiable; not done: independent review, the full decoder rule list (DJ/DR/DO style) for the entity job, byte vectors, the oracle case I/O tables, the hand-mutant to test mapping check against the E2 test style, a re-verification of every file:line after the owner's next commit (the working tree already differs from `60c9f768` in `src/memory/erasure.rs` +1 line at 31 and `src/memory/erasure/resume.rs` +77 lines at 375, so resume.rs anchors at or after line 375 shift by 77), reconciliation of entity receipts with the owner's uncommitted receipt-lookup operations (section 12).

# Gap 1 E3: entity erasure, entity barrier and edge-level chunks (draft)

Status: **DRAFT. Nothing here is approved, frozen, implemented or activated.** Every wire,
format or API shape below that an approved document does not already contain is a
PROPOSAL with options and a recommendation (section 10). Documentation only.
Base: every `file:line` was read at `60c9f768` (E2 code committed locally, not pushed) and
the tag `v0.7.1`. Line numbers are anchors at that commit, not a promise about later rebases.
Authority: [C2 remainder](gap1-c2-remainder-contract.md) section 6 E3 and decisions 4-7, 11,
12; the [E1 contract](gap1-e1-erasure-barrier-contract.md); the
[E2 contract](gap1-e2-resume-contract.md) (provisionally ratified; its persisted v2 schemas
are frozen and are not changed here); [serving contract](serving-memory-contract.md)
section 5; [Gap 1](gap1-agent-memory.md) section 5. The serving of E3 is a later slice
(section 12 lists its inputs without freezing a wire shape). The companion draft is
[E4](gap1-e4-reconciliation-contract.md).

## 1. Boundary and non-changes

E3 adds three things to the crate-private erasure core:

1. a job that erases **one entity** of an exact scope and its incident edges, with an
   **entity barrier** that is a presence probe of one exact job key, like E1's;
2. **edge-level chunks**, so an entity with more than 4,096 incident edges can finish;
3. nothing else. E2 is unchanged: its frozen job v2, its chunks and its scope-job hub
   limit (a memory with more than 4,096 incident edges stops the scope job with
   `OperationLimit`) stay as ratified. E3 lifts the hub limit for the entity case and
   gives the scope job a composition path (R6), not a changed schema.

No public Rust API, `Error` variant, wire shape, header flag, WAL operation, snapshot
encoding, Cargo or dependency change. New persisted nodes are ordinary graph nodes that
reuse the existing label `VliteMemoryErasureJob` and the reserved key prefix `receipt:v1:`
(`src/memory.rs:66-70`), so no new reserved name exists (R2). A file with no erasure data
behaves exactly as before. No crate-private `MemoryError` variant is added by E3 beyond
the E1 variants `DeletionPending` and `JobConflict` (`src/memory.rs:83`); E4 adds
`ReconciliationRequired`. Not secure erasure: receipts, outcomes, the job and the marker
stay; backups, replicas, logs and feeds keep what they held.

Not E3: fact series and temporal evidence (F6, section 6), manifests, reconciliation,
orphan sweep, receipt cleanup and the scope index (E4), session or scope targets (E1/E2),
every serving shape.

## 2. Base facts

| Fact | Evidence (verified) | Consequence |
|---|---|---|
| The barrier is a presence-only exact-key probe of the scope's singleton job node | `src/memory/erasure.rs:425` (`barrier`); `JobKey` is `operation_receipt_key("erase-scope-job", scope_key, "singleton")`, `erasure.rs:132`; label `erasure.rs:24` | Any new job node must not live at or replace the scope key's presence; a new key is invisible to old probes |
| Seven probe sites | `src/memory.rs:344`, `:349` (provision), `:378` (remember), `:589` (forget); `src/memory/consolidate.rs:525`; `src/memory/stale.rs:193`; `src/memory/recall.rs:393` | An entity probe needs the same seven sites (R3) |
| v0.7.1 contains the E1 core and these probes, not E2 | `git ls-tree v0.7.1 src/memory/erasure.rs` lists it and `src/memory/erasure/resume.rs` is absent from `src/memory/erasure/` there | 0.7.1 honours scope barriers and ignores entity jobs (section 9). **Erratum**: E2 section 7 Rollback says a build without E1 (0.7.1) ignores job nodes; 0.7.1 has E1 (ES14 in the adapter mapping states it correctly) |
| An entity is a `VliteMemory` node with `kind` `entity` and key `memory_key(scope, id)` | `src/memory.rs:186-199` (kinds), `:1603` (`memory_key`), `:1405` (`existing_memory`) | Target identity is `(scope, id)` plus the user-visible `memory_revision`, as A2 forget targets use |
| REFERENCES run from the referencing memory to any in-scope memory; cross-scope is refused | `src/memory.rs:1200-1213`; edge key `:1817` | An entity hub is the target of many incoming REFERENCES |
| A2 forget deletes every incident edge in both directions and counts guards | `src/memory.rs:655`, `:818-819` | Small entity forget exists today and is bounded by one frame |
| A2 retains evidence reachable from surviving memories | `src/memory.rs:1742` (`retained_evidence_ids`) | A job that deletes all incident edges must not retain (R7, as E2 R5) |
| C1 summaries hold DERIVED_FROM edges to their sources; S1 reads them | `src/memory/consolidate.rs:717`; `src/memory/stale.rs:26`, `:193` | Entity erasure deletes those edges; the summary becomes S1-stale (decision 12) |
| E2 chunk machinery to reuse | window `src/memory/erasure/resume.rs:41`; `scan_window` `:493`; `incident_edges` `:541` (more than 4,096 unique edges stops reading); `candidate_evidence` `:574`; `candidate_content` `:618`; `plan_chunk` `:658`; fit rule `:694`; `resume_erasure` `:720`; token `:127` | E3 changes the unit (edge, not memory) and the order, not the fence |
| Adjacency order is `(relationship type, edge ID)` ascending; `Both` merges directions and yields a self-loop once | `src/property_graph/engine/state.rs:319-321` | A deterministic edge order that a Python oracle can reproduce |
| `neighbors` pages with a cursor and a per-call examined bound | `src/property_graph/engine/query.rs:1190` | The first K edges can be read without reading the whole degree |
| A reclaim-only frame is not committed; due entities are reclaimed first in every frame | `src/property_graph/mod.rs:1133` | E2's TTL race model applies unchanged (section 8) |
| `DeleteEdge` deletes a whole temporal series | `docs/projects/roadmap/gap4-temporal-graph.md:396` | A temporal incident edge is one operation (section 6) |

## 3. Entity target and job (all PROPOSAL)

**Target.** `(scope, entity_id)`: a live memory of kind `entity` whose `memory_revision`
equals `expected_revision` (A2's rule: `RevisionConflict` otherwise). Absent or hidden by
TTL at `now`: `MissingMemory`. Present but not kind `entity`: `InvalidTarget`.

**Keys.** `EntityJobKey = R("erase-entity-job", entity_id)` and receipt keys
`R("begin-entity-erasure", key)`, `R("resume-entity-erasure", key)`, with the E1 `R` helper
and outcome key (`receipt_outcome_key`). One job per `(scope, entity_id)` forever.

**Job node** (`VliteMemoryErasureJob`, schema `erase-entity:v1`; exactly these properties,
no TTL, no edge, no attachment): Strings `e1_schema`, `scope`, `e1_scope_tuple`,
`e1_collection`, `begin_digest`, `generation` (canonical nonzero u64), `state`
(`pending`|`logical`), `logical_visibility` (`hidden`), `deleted_count` (u64 units),
`remaining` (`unknown`|u64), `e3_chunks` (u64, 0 before the first chunk), `e3_entity_id`,
`e3_entity_revision` (u64 at begin), `e4_lineage` (16 lowercase hex, nonzero; written by
E4's rules, see E4 section 4); Integer `created_at_ms`. DJ-style decoder rules follow E2
section 4: label, exact names and types, string bounds; `e1_scope_tuple` mismatch is
`DigestCollision`; `state` is `logical` if and only if `remaining` is `0`;
`deleted_count` is at most 4,092 times `max(e3_chunks,1)`. *Not done: the numbered
DJ/DR/DO rule list and the byte vectors.*

**Begin frame** (one frame, ops in this order): job `CreateNode`, outcome `CreateNode`,
receipt `CreateNode`, and the scope's entity marker `CreateNode` when absent (R3). Receipt
`e1_schema` `erase-entity-begin:v1`; outcome is E1's five LF lines with schema
`erase-entity:v1` and `EntityJobKey`.

**Digests (domain strings).** `vlite.memory.erasure.entity.begin.v1\0` over `SCOPE ||
O(collection) || S(entity_id) || U64(expected_revision) || U64(receipt_window_ms)`;
`vlite.memory.erasure.entity.token.v1\0` over `SCOPE || O(collection) || S(EntityJobKey) ||
U64(generation) || U64(job_revision) || U64(chunks)` (a distinct domain, so a scope token
can never validate an entity job); `vlite.memory.erasure.entity.resume.v1\0` as E2's resume
digest with `EntityJobKey`. Chunk outcome: seven LF lines `erase-entity-chunk:v1`,
`EntityJobKey`, `chunk_no`, `units_deleted`, `deleted_total`, `remaining|unknown`,
`pending|logical`.

**Begin order.** Structural validation as E1 begin, plus `entity_id` opaque (`InvalidKey`);
read the scope node (`InvalidScope`, `DigestCollision`); probe the scope job: present is
`DeletionPending` unless R6 allows the composition; probe and decode the entity job and the
begin receipt (live same digest replays, other digest `IdempotencyConflict`, a job with no
live receipt `JobConflict`); deadline; read the entity (`MissingMemory`, `InvalidTarget`,
`RevisionConflict`); frame.

**Coexistence.** Entity inside a barred scope: R6. Scope erasure over a scope that holds an
entity job (pending or logical): allowed (the scope job needs only the scope node and no
scope job); the scope chunks delete every live memory including a pending entity; a later
entity resume finds the entity absent and completes with 0 units (state `logical`). Two
entity jobs: independent keys; a shared edge is deleted by whichever chunk runs first.
Same entity twice: `JobConflict` or replay as E1.

## 4. Entity barrier (PROPOSAL)

Presence of `EntityJobKey` is the barrier, permanent, no TTL (C2 decision 5). Effects, each
a `DeletionPending` unless noted, evaluated after the scope barrier:

| Hook | Entity barrier effect |
|---|---|
| remember | An item whose id is a barred entity (create, merge or dedupe), a `references` entry or a `merge_into` naming one |
| forget (A2) | A target naming the barred entity (the job is its deletion path). Targets that merely reference it are allowed: deleting their edges is progress |
| consolidate | An input or the output id naming a barred entity |
| recall | The entity is never a hit, a transit node or a vector/text candidate (filtered before ranking, like a scope); a seed naming it is `DeletionPending` after the scope loop; `deletion_barriers` is not reused, a second crate-private list `entity_barriers` is proposed |
| summary_status (S1) | Option: a barred source counts as changed (stale); otherwise unchanged (R4) |
| provision_scope, erasure reads | unaffected |

**Probe cost (R3).** The E1 hooks probe one exact key. remember and forget already scan
every memory of the scope (`src/memory.rs:1375`), and recall scans label `VliteMemory`
(`src/memory/recall.rs:403`), so one exact probe per named id costs the same order as work
those hooks already do. recall needs one probe per eligible entity; a per-scope **entity
marker** (`R("erase-entity-marker","singleton")`, schema `erase-entity-index:v1`, created
in the first entity begin of the scope, presence only) gates those probes so a scope with
no entity job pays one extra exact-key probe per hook and nothing per id. Options: M1
marker-gated (recommended), M2 probe always, M3 a flag property on the entity node (zero
extra reads, but it mutates an accepted A1 node and creates a second source of truth).

## 5. Edge-level chunks (PROPOSAL)

A **unit** is one incident edge (one operation) or one HAS_CONTENT edge with its content
node (two operations, an atomic pair so no content node is ever left edgeless). Content
nodes are validated exactly as E2 step 3 (`resume.rs:618`). Temporal edges are ordinary
units (section 6).

- **Window.** The first 4,097 incident edges in adjacency order, read in pages, no stored
  cursor (deleted edges vanish). `remaining` is known only when the window holds fewer
  than 4,097 entries; it counts units plus one while the entity node exists, so `state` is
  `logical` if and only if `remaining` is `0`.
- **Fit rule.** Non-final chunk: `3 + unit_ops + 1 <= 4,096` (job upsert, outcome,
  receipt, job guard), so at most 4,092 unit operations. Final chunk (the entity node, its
  evidence ops and every remaining unit fit): `3 + unit_ops + 1 (node) + evidence_ops +
  2 (guards: entity, job) <= 4,096`. The first unit always fits, so only read ceilings
  (`OperationLimit`) and structural stops (`ForeignAttachment`, `InvalidBundle`,
  `InvalidEvidence`) stop an entity job. A hub of any degree finishes.
- **Frame order.** Evidence `Detach` then `Vector::Delete` (final chunk only), `DeleteEdge`
  per unit in window order with `Expected::Revision`, content `DeleteNode`, entity
  `DeleteNode` (final only), job `UpsertNode` with `Expected::Revision(job revision)`,
  outcome, receipt last. Expired same-key receipts are replaced as E2 does.
- **Fence.** E2 steps 1-11 with entity substitutions: job revision read fresh, generation,
  token of section 3; replay returns the stored view and ignores deadline and fence;
  `JobConflict` for every fence failure; `MissingMemory` for a missing job or another key.
- **Scope-job hub (R6).** Unchanged. Remedy: an entity job for the hub, allowed inside the
  barred scope, then resume the scope job.

## 6. What is deleted and what is not

Deleted per entity job: every incident edge of any type (REFERENCES, DERIVED_FROM,
HAS_CONTENT, others; a temporal series whole by one `DeleteEdge`), content nodes, the
entity's evidence record (detached then deleted, never retained, R7) and the entity node.
Not deleted: memories that referenced the entity (they survive; their `reference_count`
counts references made and is not decremented), summaries (they become S1-stale), other
entities, receipts of other operations, and evidence records referenced from fact-series
segment properties (F6 owns them; E3 neither reads nor deletes them). The F6 note in the C2
contract says E3 handles temporal incident edges: E3 deletes the edge and stops there, and
whether `neighbors` lists temporal edges at a current clock is **not verified** here
(open question 5 in the notes).

## 7. Small forget versus the job

A2 `forget` by ID already deletes a small entity atomically with no barrier and retains
evidence by reachability (`src/memory.rs:578`, `:818-819`, `:1742`). The threshold is A2's
own fit. The job differs in three ways: it leaves a permanent barrier on the ID (an erased
entity id cannot be recreated in that scope), it never retains evidence, and it writes a
manifest in E4. R8 asks whether serving routes every entity target to the job. The adapter
chooses; the core offers both.

## 8. Atomicity, TTL, crash and reopen

One chunk is one frame; planning and refusals change nothing; `CommitIndeterminate` is
preserved. The commit clock is `max(now, wall, graph high-water)`; due entities are
reclaimed first, so a unit that expires in `(now, commit clock]` fails the frame with a
`Storage` error (E2 section 7 model, same test seam). An entity that expires between
chunks is reclaimed with its edges; the next chunk completes with 0 units. Reopen is
acceptance on plain file, encrypted file, directory store and encrypted directory store with
a reopen between every step and after a pre-append failure. Compaction, checkpoint and
upgrade relabel graph revisions (`src/maintenance.rs:940` and its doc comment,
`docs/projects/roadmap/p2-directory-store.md:137`); the fence reads the job revision fresh,
so a relabel never makes a chunk unsafe; E4 owns the rest.

## 9. Maintenance and rollback

0.7.1 ignores entity jobs and the marker: the entity stays readable and referenceable, new
edges can be added, and the entity id can be recreated. After rolling forward a pending job
continues over the new edges; a `logical` job hides a recreated entity (the barrier is by
id). Do not roll back while any entity job exists. Every new node has label
`VliteMemoryErasureJob` and prefix `receipt:v1:`, so 0.7.1's reserved-name guard and its
memory-only adoption check (`src/memory/names.rs:15`, `:21`; `src/memory/startup.rs:159`)
stay valid. The scope job's presence probe is untouched.

## 10. Ruling table (all PROPOSAL, status: open)

| # | Question and options | Recommendation |
|---|---|---|
| E3-R1 | Entity identity: `(scope, id)` plus memory revision; kind must be `entity` | Adopt as written |
| E3-R2 | Where entity jobs live: (a) existing label and prefix, new schema string; (b) new label (breaks 0.7.1 legacy adoption scan for record-less graphs, needs a `MEMORY_NODE_KINDS` entry) | (a) |
| E3-R3 | Barrier probe: M1 marker-gated, M2 probe always, M3 flag on the entity node | M1 |
| E3-R4 | S1 on a barred source: report stale, or unchanged until the edge is deleted | Report stale (bounded by 256 sources) |
| E3-R5 | Entity barrier permanence: (a) permanent like the scope barrier, (b) lifts at `logical`, (c) lifts after retention | (a); an erased id cannot be recreated; a later explicit reactivation is additive. **Product decision for the coordinator** |
| E3-R6 | Entity begin and resume inside a scope that holds a scope job: refuse (`DeletionPending`), allow (erasure operations are exempt), or lift the hub limit inside the scope job (needs a v3 outcome, DO3 forbids a 0-memory chunk) | Allow |
| E3-R7 | Entity evidence: always delete (E2 R5) or A2 retention | Always delete |
| E3-R8 | Serving routes every entity target to the job, or small ones to A2 | Coordinator and ES |
| E3-R9 | Unit and window sizes (section 5), `remaining` counts units plus one | Adopt |
| E3-R10 | `e4_lineage` and `generation` are part of `erase-entity:v1` from the start | Adopt; any later schema change is a new version |

## 11. Acceptance and test plan

Independent oracle (a later author; names only): `tests/memory_erasure_entity_oracle.py`
with fixture `tests/fixtures/agent_memory/erasure_entity_golden.json`, run normal and `-O`.
Named golden cases: `entity-begin-vector`, `entity-begin-replay`, `entity-begin-digest-conflict`,
`entity-begin-missing`, `entity-begin-not-entity`, `entity-begin-stale-revision`,
`entity-begin-in-barred-scope`, `entity-begin-barred-scope-composition`, `entity-job-decoder-<rule>`
(one per rule), `entity-barrier-remember-item`, `entity-barrier-remember-reference`,
`entity-barrier-forget-target`, `entity-barrier-consolidate-input`, `entity-barrier-recall-hit`,
`entity-barrier-recall-seed`, `entity-barrier-s1-source`, `entity-barrier-sibling-entity-free`,
`entity-marker-gate`, `entity-chunk-small-final`, `entity-chunk-hub-5000`,
`entity-chunk-fit-4092-4093`, `entity-chunk-final-fit-boundary`, `entity-chunk-content-pair`,
`entity-chunk-shared-edge-two-jobs`, `entity-chunk-temporal-edge`, `entity-evidence-always-deleted`,
`entity-evidence-foreign-attachment-stops`, `entity-summary-becomes-stale`,
`entity-referencers-survive`, `entity-replay-original-view`, `entity-expired-key-reuse`,
`entity-resume-entity-gone`, `entity-ttl-race`, `entity-reopen-four-backends`,
`entity-maintenance-relabel-checkpoint`, `entity-maintenance-relabel-compaction`.
Inputs and outputs are the E2 oracle's shapes with entity fields; a Python snapshot round
trip is logical evidence, not backend evidence.

Hand mutants, each with the test that must kill it (Rust names proposed in
`src/memory/erasure/entity_tests.rs`): 1 ignore the scope barrier before the entity begin
(`entity_begin_in_barred_scope`); 2 skip the entity probe in remember
(`entity_barrier_remember_item`); 3 skip it for references
(`entity_barrier_remember_reference`); 4 skip it in forget, consolidate, recall hit,
recall seed, S1 (one test each); 5 probe the marker but not the entity key
(`entity_marker_gate`); 6 omit the marker create at begin (`entity_marker_gate`); 7 count a
content pair as one operation (`entity_chunk_content_pair`); 8 fit off by one at 4,092 or
4,093 (`entity_chunk_fit_4092_4093`); 9 delete the node in a non-final chunk
(`entity_chunk_final_fit_boundary`); 10 order edges by ID instead of `(relationship, id)`
(`entity_chunk_order`); 11 skip temporal edges (`entity_chunk_temporal_edge`); 12 retain
evidence by A2's rule (`entity_evidence_always_deleted`); 13 delete only the entity (dangling
edges); 14 delete content node without its edge or the reverse; 15 accept a scope token for an
entity job (`entity_token_domain`); 16 ignore `expected_revision`; 17 mark `logical` while
`remaining` is unknown; 18 replay returns the current job; 19 resume a `logical` job;
20 delete referencing memories; 21 decrement a referencer's `reference_count`; 22 chunk
reads the whole degree instead of the window (hub test with a read-ceiling harness). Two
mutants are expected equivalent under `&mut Database` (frame guards, as E2) and are
recorded, not killed.

## 12. Inputs the serving adapter needs (not a wire shape)

Core input to begin: scope, entity id, expected memory revision, idempotency key,
`receipt_window_ms`, `now_ms`, optional `deadline_ms`, plus E4's retention input. Core
output: job key (the `job_id`; amends ES1, because an entity job is not the scope's
singleton), revision, generation, state, hidden visibility, `deleted_count`,
`remaining`, chunks, resume token, target echo `(entity id, revision)` (amends ES2),
`manifest_id` (E4), `replayed`. Resume input: entity job key, revision, generation,
token, key, window, clock, deadline. Errors: the existing core variants (E4 adds
`ReconciliationRequired`). Entity-barred recall omission needs an `entity_barriers` list for
`meta.truncation`.

Receipt lookup (open, not reconciled): the owner's uncommitted working tree on 2026-11-06
(not part of `60c9f768`, so not a base fact) adds `ReceiptOperation::BeginErasure`
(`begin-erasure`) and `ReceiptOperation::ResumeErasure` (`resume-erasure`) in
`src/memory/mapping.rs`, `ReceiptOutcome::BeginErasure { job }` and
`ReceiptOutcome::ResumeErasure { job, chunk }` in `src/memory/receipt.rs`, and
`lookup_begin` and `lookup_chunk` in `src/memory/erasure/resume.rs`. This draft does not
reconcile entity begin and entity chunk receipts with that dispatch. PROPOSAL, unverified
against the uncommitted code: entity receipts reuse the two operations and the job view
carries the target (scope or entity); the alternative is two new operation names. The
coordinator or the owner decides when the ES slice lands.

## 13. Not frozen

Everything in this document, and in particular: R1-R10, the entity job and marker schemas,
digests and outcome text, the barrier hook list, the unit and window rules, the oracle
case list, the mutant list, serving, F6 temporal evidence, multi-worker claims and any
secure-erasure, backup or replica claim.
