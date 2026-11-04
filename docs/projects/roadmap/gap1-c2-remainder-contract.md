# Gap 1 C2: slice C remainder scoping contract (proposed)

Status: **scoping accepted by the coordinator on 2026-11-04; nothing here is
frozen or implemented.** The numbered items in section 7 stay proposals except
where section 9 records a provisional ruling. Nothing here authorizes code, an
oracle, a test or an edit to another document by itself: each sub-slice needs its
own authorization, and the E, F and X1 slices also need their own frozen contract
and independent oracle first (section 5, rule 6).
Base: every file:line reference was checked against commit
`1acd3ba041b8e75e1388fcde701299b3899b10cb`; line numbers are anchors at that
commit, not a promise about later rebases.

This document splits what remains of Gap 1 slice C after the integrated C1
consolidation ([frozen C1 contract](gap1-c1-consolidation-contract.md)) into
the smallest sub-slices that can each be accepted on their own. Authority stays
with [Gap 1](gap1-agent-memory.md) sections 1, 4, 5 and 9; this document does
not override it, the C1 contract, [Gap 4](gap4-temporal-graph.md),
[Gap 2 ledger](gap2-lease-ledger.md) or the
[memory serving contract](serving-memory-contract.md).

## 1. Boundary and non-changes

Every sub-slice in this document is crate-private and inactive, like A1, A2, B
and C1. Explicitly, no sub-slice proposed here makes any of these changes:

- no public Rust API, public `Error` variant or public type change;
- no wire, REST, gRPC, MCP, CLI, binding, client, OpenAPI or proto change;
- no on-disk format, header flag, WAL operation or snapshot encoding change;
  memory state stays ordinary graph nodes, edges, attachments and (for facts)
  the existing Gap 4 temporal entries;
- no new crate dependency and no `Cargo.toml` change;
- no activation or gate change: the temporal gate stays closed in release
  builds, and no default changes;
- no private worker pool, background thread, scheduler, timer or retry loop;
  every step runs inside one caller-invoked call under `&mut Database` or
  `&Database`;
- no performance, latency, throughput, memory-use or storage-size claim; the
  relative sizes in section 6 compare sub-slices with each other only.

Serving (Gap 1 slice D) remains after all of C, as the delivery boundary
already requires (`docs/projects/roadmap/serving-memory-contract.md:934`).
Approving any sub-slice here does not approve any serving schema.

Crate-private additions to the private `MemoryError` enum
(`src/memory.rs:45-80`) are proposals (decision 19), not public errors.

## 2. Base facts that bound this scoping

| Base fact | Evidence at `1acd3ba0` | Consequence |
|---|---|---|
| C1 is implemented and integrated, crate-private | `src/memory.rs:9`; `src/memory/consolidate.rs:489`; `docs/projects/roadmap/next-major.md:323-326` | The remainder starts after C1. Note: `docs/projects/roadmap/gap1-agent-memory.md:314-316` still says C1 "is not implemented"; this document does not edit it (decision 23) |
| C1 accepts only the retain disposition | `src/memory/consolidate.rs:34-38`; `src/memory/consolidate.rs:187`; `docs/projects/roadmap/gap1-c1-consolidation-contract.md:42,74` | A delete disposition is a contract change and needs a new freeze |
| C1 writes one DERIVED_FROM edge per source, carrying the source memory revision | `src/memory/consolidate.rs:685-696` | Staleness can be derived from retained edges, without writes |
| Small forget deletes every incident edge of a target, in both directions | `src/memory.rs:505`; `src/memory.rs:559`; `src/memory.rs:573` | Forgetting a source silently drops the summary's DERIVED_FROM edge; nothing reports it today |
| Forget targets are exact ID plus revision only; oversized erasure rejects | `src/memory.rs:200-203`; `src/memory.rs:573`; `docs/projects/roadmap/gap1-agent-memory.md:354-357` | Session, scope, entity and fact-series erasure are all remainder |
| Memory kinds are Entity, Episodic and a refusal marker | `src/memory.rs:141-145` | Fact and Relation kinds are remainder; adding a kind reopens recall transit filtering (`docs/projects/roadmap/gap1-agent-memory.md:298`) |
| Every operation reads the provisioned scope node first | `src/memory.rs:290`; `src/memory.rs:505-514`; `src/memory/consolidate.rs:505-509`; `src/memory/recall.rs:354-357` | A scope barrier check has one natural, already-read anchor per operation |
| Recall expands REFERENCES only; temporal expansion is used only on temporal files | `src/memory/recall.rs:754`; `src/memory/recall.rs:789-790` | Fact segments and DERIVED_FROM are not traversed by recall today |
| Temporal operations exist inside ordinary graph batches | `src/property_graph/mod.rs:354`; `src/property_graph/temporal.rs:56-103` | Facts and corrections need no new engine operation |
| Temporal support is false outside this crate's tests | `src/property_graph/temporal.rs:123-126`; `src/property_graph/temporal.rs:268-278`; `src/property_graph/temporal.rs:285-298` | Fact sub-slices are testable only through the existing test seam; production stays refused |
| Private as-of reads and bounded timelines exist | `src/property_graph/temporal/read.rs:91-98`; `src/property_graph/temporal/read.rs:266-270`; `src/property_graph/engine/temporal.rs:24` | Fact query/history can reuse them with the 256-segment cap |
| An edge created earlier in a batch cannot be corrected in that batch | `docs/projects/roadmap/gap4-temporal-graph.md:501` | A replacement series is created with its full series, not created then corrected |
| Public KV has CAS versions and expiry; listing scans the collection | `src/lib.rs:34`; `src/kv.rs:9-15`; `src/kv.rs:17-19` | Usable for claims, but a KV write is not atomic with a graph frame (`docs/projects/roadmap/gap1-agent-memory.md:119`) |
| The fenced-lease ledger is design-only; Phase 2 is gated | `docs/projects/roadmap/gap2-lease-ledger.md:3`; `docs/projects/roadmap/gap2-lease-ledger.md:27` | No sub-slice may depend on the ledger |

## 3. Remainder inventory

| Remainder item (source) | Sub-slice |
|---|---|
| Derived summaries marked stale when discovered, bounded reporting (`gap1-agent-memory.md:333-334`; `serving-memory-contract.md:343-344`) | S1, S2 |
| Explicit non-retain disposition, dry-run plan (`gap1-agent-memory.md:305-308`; `serving-memory-contract.md:337-341`) | X1 (deferred) |
| Scope/session deletion barrier and job (`gap1-agent-memory.md:358-363`; `serving-memory-contract.md:393-401`) | E1 |
| Fenced, resumable chunks with receipts and progress (`gap1-agent-memory.md:361-367`; `serving-memory-contract.md:403-411`) | E2 |
| Entity erasure with bounded incident edges (`gap1-agent-memory.md:353`; `serving-memory-contract.md:382-383`) | E3 |
| Reconciliation after maintenance; deletion manifests (`gap1-agent-memory.md:374-376`; `serving-memory-contract.md:410-411`) | E4 |
| Fact slots and fact/relation remember (`gap1-agent-memory.md:86-95`; `gap4-temporal-graph.md:137`) | F1 |
| Revision-checked correction and supersession (`gap1-agent-memory.md:319-327`; `gap4-temporal-graph.md:130-136`) | F2 |
| Fact query at T and bounded history (`serving-memory-contract.md:321-325`) | F3 |
| Disputed assertions and structural contradiction labels (`gap1-agent-memory.md:329-332`) | F4 |
| Recall over fact segments (`gap1-agent-memory.md:437-438`; `gap1-agent-memory.md:298`) | F5 |
| Forget a whole fact series; entity forget with temporal edges (`gap1-agent-memory.md:349-353`; `gap4-temporal-graph.md:221-222`) | F6 |
| Crash/retry and maintenance tests for all of the above (`gap1-agent-memory.md:445`; `gap1-agent-memory.md:496`) | Every slice, E4 |

## 4. Dependency classification

Classification is from the base code, not from roadmap intent.

| Dependency | State at base | Class | Effect |
|---|---|---|---|
| Gap 1 A1, A2, B, C1 | Integrated, private (`next-major.md:320-326`) | Satisfied | All tracks may start |
| Gap 3 graph TTL | Used by integrated A2 forget/expiry | Satisfied | Barrier and job nodes carry no graph expiry (decision 5) |
| Gap 4 A/B writes, C1/C2 reads | Integrated, private, gated (`gap4-temporal-graph.md:507-509`; `src/property_graph/temporal.rs:268-278`) | Satisfied for private tests only | F1-F6 unblocked as crate-private code exercised with the test seam; every release path keeps `UnsupportedFeature(64)` |
| Gap 4 D (Gap 1 integration) | Not started (`gap4-temporal-graph.md:283`) | Co-delivered, not a prerequisite | F1-F6 are the content of Gap 4 D; recording that is decision 20 |
| Gap 4 activation/qualification | Not qualified; nothing active (`next-major.md:63-66`) | Blocks production use only | No fact behaviour is reachable outside tests; no slice changes this |
| Gap 2 KV API | Public, CAS-versioned (`src/kv.rs:9-15`) | Available, not required | Not used by any proposed slice (decision 6); never a job planner or completion proof (`gap1-agent-memory.md:367`) |
| Gap 2 fenced-lease ledger | Design accepted, Phase 2 gated, no code (`gap2-lease-ledger.md:3,27`) | Unavailable | No dependency; fencing uses the graph job revision and generation in the same frame |
| Gap 12 / serving approval | Serving waits for A-C (`serving-memory-contract.md:934`) | After | Not touched |

Track dependencies inside C2:

| Sub-slice | Depends on | Unblocked at base? |
|---|---|---|
| S1 | C1 | Yes |
| S2 | S1 | After S1 |
| X1 | S1, decision 13, a new C1-style freeze | No (deferred) |
| E1 | A2 | Yes |
| E2 | E1 | After E1 |
| E3 | E2 | After E2 |
| E4 | E2 | After E2 |
| F1 | Gap 4 A-C2 (satisfied), decisions 14-16 | Yes, after decisions |
| F2 | F1 | After F1 |
| F3 | F1 | After F1 |
| F4 | F1, F3 | After F3 |
| F5 | F3 | After F3 |
| F6 | F1; E3 if E3 lands first | After F1 |

The S, E and F tracks are logically independent but share hook files
(`src/memory.rs`, `src/memory/recall.rs`, `src/memory/consolidate.rs`). Merge
sub-slices touching the same hook file serially; no slice may rewrite another's
accepted hunk.

## 5. Rules shared by every sub-slice

1. **Files.** New logic lives in a new submodule beside `src/memory/consolidate.rs`
   (`src/memory/<name>.rs` with tests in `src/memory/<name>/tests.rs`).
   Edits to existing files are limited to the named hook lines: a `mod`
   declaration, a call, or a crate-private enum variant. Existing tests,
   accepted oracles and accepted fixtures are not edited.
2. **Atomicity.** Each mutation, its receipt and every guard share one
   `GraphMut::write_batch_at` frame, as A2 and C1 do. Validation, planning and
   preappend refusals change nothing. `CommitIndeterminate` is preserved,
   never retried blindly (`gap1-c1-consolidation-contract.md:344-348`).
3. **Receipts.** New operations use new versioned operation names in the
   existing `operation_receipt_key` scheme and a length-delimited binary
   digest with its own domain string, following C1 section 3. A1/A2/C1 bytes
   do not change.
4. **Bounds.** 4,096 user operations and 16 MiB per frame, counted as A2/C1
   count them; reads charge every examined edge. Over-cap work rejects or, for
   jobs only, stops at a chunk boundary; nothing silently truncates.
5. **Fail-first.** After implementation, each behaviour group must have a real
   stub or mutation that makes its tests fail for the intended reason, then
   pass when restored. Storage-invariant-equivalent or unexecuted mutations
   are not counted as kills.
6. **Oracle ownership.** Where a sub-slice requires an independent oracle, a
   reviewer other than the implementer writes the oracle and its fixtures in a
   separately authorized oracle slice first. The implementer replays them and
   never edits them. Fixtures are additive; the accepted
   `tests/agent_memory_oracle.py`, `tests/memory_ranking_oracle.py`,
   `tests/c1_consolidation_oracle.py` and their goldens are unchanged. CI
   wiring of a new oracle is one added line in the oracle list at
   `.github/workflows/ci.yml:29-31`, owned by the oracle slice.
7. **Reopen.** Every persisted object round-trips on plain file, encrypted
   file, directory store and encrypted directory store, as C1 requires.
8. **Temporal seam.** Fact tests use the existing test-only `Activation`
   (`src/property_graph/temporal.rs:285-298`) and `enable_temporal` copy-out,
   as accepted B did; no gate or production path changes.

## 6. Sub-slices

Relative size: XS < S < M < L, compared only with each other (C1 would be L).
They are not effort, schedule, code-size or performance estimates.

### S1: stale-summary status (read-only)

- **Goal.** A bounded, read-only status for one C1 summary under captured C.
- **Allowed files.** New `src/memory/stale.rs`, new `src/memory/stale/tests.rs`;
  `src/memory.rs` one `mod stale;` line.
- **Private surface (proposed).** `MemoryRef::summary_status(&SummaryStatusRequest)
  -> MemoryResult<SummaryStatus>`; `SummaryStatus { summary_id, source_count,
  live_sources, changed_sources, expired_sources, stale: bool, view_sequence }`.
- **Contract.** Per decision 2, stale iff any of: retained DERIVED_FROM edges
  fewer than `c1_source_count`; an edge's `c1_source_memory_revision` differs
  from the source's current `memory_revision`; a source hidden by TTL at C.
  Strict decoding of the C1 summary schema; a non-C1 node is `InvalidBundle`.
  At most 256 edges examined; no write, no repair, no cascade.
- **Tests.** Fresh summary not stale; source forgotten; source merged
  (revision bump); source expired at C-1/C/C+1; summary expired; malformed
  edge revision; non-C1 node; out-of-scope ID; four-backend reopen equality.
- **Fail-first.** Stub returning `stale=false` fails the forget/merge/expiry
  cases; dropping the TTL check fails only the expiry case.
- **Oracle.** Not required (decision 21); engine tests only.
- **Size.** S.
- **Acceptance.** All tests above pass; no file outside the allowed list
  changes; C1 golden replay unchanged.

### S2: staleness label in recall

- **Goal.** Recall hits that are C1 summaries carry the S1 stale label.
- **Allowed files.** `src/memory/recall.rs` (result field and one call);
  `src/memory/stale.rs`; new tests in `src/memory/stale/tests.rs`.
- **Private surface.** One crate-private `stale: Option<bool>` on the recall
  hit (`None` for non-summaries).
- **Contract.** Label examination is charged to recall's graph budget; budget
  exhaustion follows existing `allow_partial` rules, never a guessed label.
  Ranking, eligibility and ordering are unchanged.
- **Tests.** Label true/false/None; budget charge at exact boundary; partial
  versus refusal; ranking golden unchanged.
- **Fail-first.** Omitting the charge fails the boundary test; a constant label
  fails the forget case.
- **Oracle.** Not required; `tests/memory_ranking_oracle.py` replay must stay
  exact.
- **Size.** XS.
- **Acceptance.** Existing recall tests unchanged and passing.

### X1: consolidation delete disposition (deferred)

- **Goal.** Explicit per-input logical delete during consolidation.
- **Status.** Not proposed for authorization now. Deleting a source in the
  same frame removes its DERIVED_FROM edge (no dangling edges), which S1 would
  then report as stale. Resolving that needs a new schema and digest version
  and a new freeze (decision 13).
- **Allowed files when authorized.** `src/memory/consolidate.rs`, a new test
  file under `src/memory/consolidate/`, and a new frozen contract document.
- **Oracle.** Required; a new additive fixture file, not the accepted C1 golden.
- **Size.** M.

### E1: scope/session deletion barrier and job record

- **Goal.** Commit an erasure job for one exact scope (a session is a scope
  tuple), after which every memory read and write in that scope refuses or
  hides, with no data deleted yet.
- **Allowed files.** New `src/memory/erasure.rs`, new
  `src/memory/erasure/tests.rs`; hooks: `src/memory.rs` (`mod erasure;`, barrier
  check in `remember`, `forget`, `provision_scope`, new private error
  variants), `src/memory/consolidate.rs` (barrier check),
  `src/memory/recall.rs` (barrier check and diagnostic).
- **Private surface (proposed).** `MemoryMut::begin_erasure(&EraseScopeRequest)
  -> MemoryResult<ErasureStatus>`, `MemoryRef::erasure_status(..)`;
  `ErasureStatus { job_key, revision, generation, state, remaining:
  Option<u64>, resume_token }`; variant `DeletionPending`.
- **Contract.** One frame creates the job node (decision 4), its receipt and
  outcome. The barrier is the job node's presence, checked immediately after
  the existing scope-node read. Writes in the scope fail `DeletionPending`
  before receipt lookup of a different operation; receipt replay of an
  operation committed before the barrier still replays (decision 5). Recall
  omits the scope and reports a `deletion_barrier` diagnostic. Only the exact
  scope is barred; child scopes are not.
- **Tests.** Barrier then remember/forget/consolidate/recall in that scope;
  sibling and child scopes unaffected; begin replay and digest conflict;
  preappend failure leaves no barrier; four-backend reopen.
- **Fail-first.** Removing each operation's check fails exactly that
  operation's test.
- **Oracle.** Required (decision 21): new `tests/memory_erasure_oracle.py` and
  `tests/fixtures/agent_memory/erasure_golden.json`, written first by an
  independent author; E1 replays its barrier cases.
- **Size.** M.
- **Acceptance.** Oracle replay exact; no deletion occurs in E1.

### E2: fenced resume chunks

- **Goal.** `resume_erasure` deletes one bounded, deterministic chunk per call
  and advances the job in the same frame, until the scope holds no memory.
- **Allowed files.** `src/memory/erasure.rs`, `src/memory/erasure/tests.rs`;
  `src/memory.rs` only to make existing forget planning helpers callable from
  the submodule (visibility, no behaviour change); new error variant.
- **Private surface (proposed).** `MemoryMut::resume_erasure(&ResumeRequest)
  -> MemoryResult<ErasureStatus>`; variant `JobConflict`.
- **Contract.** The request supplies job revision, generation and token; the
  frame guards the job node with `Expected::Revision`, so a stale token,
  generation or concurrent chunk cannot commit (decision 6). Chunk order is
  canonical memory-key order; each chunk reuses A2's detach, shared-evidence
  and foreign-attachment rules and fits one frame including progress and
  receipt (decision 7). Replaying a resume key returns its original chunk
  result. `remaining` is exact only when counted within budget, else `None`
  (decision 8). Completion sets state `logical`; the barrier stays (decision 5).
  Partial completion is never reported as complete.
- **Tests.** Multi-chunk scope to completion; replay of each chunk; stale
  revision, generation and token; crash before append and after append with
  reopen; foreign attachment stops the job with an explicit error, not a skip;
  shared evidence retained; exactly-at-cap chunk.
- **Fail-first.** Removing the job revision guard fails the stale-token test;
  unbounded chunk planning fails the cap test.
- **Oracle.** Same erasure oracle; chunk/resume cases.
- **Size.** L.
- **Acceptance.** Oracle replay exact; no atomic-job claim in any message.

### E3: entity erasure

- **Goal.** Barrier and chunked erasure for one entity anchor and its bounded
  incident edges, in the E1/E2 job model.
- **Allowed files.** `src/memory/erasure.rs`, `src/memory/erasure/tests.rs`;
  barrier check hooks as in E1.
- **Contract.** Per decision 12: the entity barrier hides the entity and makes
  writes referencing it fail; REFERENCES, DERIVED_FROM and HAS_CONTENT edges
  are deleted per chunk; summaries that lose a source become S1-stale, not
  deleted.
- **Tests.** High-degree entity across chunks; summary staleness after entity
  erasure; references from other memories removed; replay and reopen.
- **Fail-first.** Skipping the incident-edge pass fails the dangling-edge check.
- **Oracle.** Erasure oracle, entity cases.
- **Size.** M.

### E4: reconciliation and deletion manifests

- **Goal.** Jobs fail closed after revision-changing maintenance, and each job
  keeps a manifest without content.
- **Allowed files.** `src/memory/erasure.rs`, `src/memory/erasure/tests.rs`;
  new error variant in `src/memory.rs`.
- **Contract.** A job records its lineage-relative anchors; after compaction
  or upgrade renumbers revisions, `resume_erasure` returns
  `ReconciliationRequired` until a private `reconcile_erasure` re-reads the job
  and advances its generation (decision 10). A manifest node stores target
  identities and policy only, never content, vectors or source URIs, with
  explicit retention (decision 11). No secure-erasure, backup or replica claim.
- **Tests.** Compaction, upgrade, directory checkpoint (no renumbering, so no
  reconciliation), encrypted variants; manifest field allowlist.
- **Fail-first.** Accepting a pre-compaction token fails the reconciliation test.
- **Oracle.** Erasure oracle, reconciliation cases.
- **Size.** M.

### F1: fact slots and fact/relation remember

- **Goal.** Caller-declared exclusive or multivalued slots and creation of
  temporal fact series between entity anchors.
- **Allowed files.** New `src/memory/facts.rs`, new `src/memory/facts/tests.rs`;
  `src/memory.rs` (`mod facts;`, `MemoryKind` variants, private error
  variants); `src/memory/recall.rs` only to keep refusing the new kinds until
  F5 (one match arm).
- **Private surface (proposed).** `MemoryMut::provision_slot(&SlotSpec)`,
  `MemoryMut::assert_facts(&FactRequest) -> MemoryResult<FactResult>`;
  `MemoryKind::{Fact, Relation}`; variants `SlotConflict`, `SlotModeConflict`.
- **Contract.** Per decisions 14-16: a slot node keyed by the scope and the
  `(subject, predicate, slot_key)` digest stores its immutable mode and a
  bounded series list; every fact write upserts the slot node with
  `Expected::Revision`, so concurrent writers serialize on it. A series is a
  keyed `TemporalOp::CreateEdge` between in-scope entity anchors; exclusive
  slots reject any overlap across all of the slot's series (touching legal);
  the 256-segment cap and half-open bounds come from Gap 4. Disagreement
  defaults to reject. Validity is never read from ordinary properties.
- **Tests.** Exclusive overlap, touching, gap; multivalued parallel values;
  slot revision race; cross-scope endpoints; i64 extremes; 256/257 segments;
  4,096/4,097 operations; refusal without the seam (`UnsupportedFeature(64)`,
  nothing written); four-backend reopen.
- **Fail-first.** Dropping the slot guard fails the race test; checking only
  the newest series fails the exclusive-overlap test.
- **Oracle.** Required: new `tests/memory_fact_oracle.py` and
  `tests/fixtures/agent_memory/fact_golden.json` by an independent author; the
  accepted `tests/temporal_interval_oracle.py` stays unchanged.
- **Size.** L.

### F2: corrections and supersession

- **Goal.** Revision-checked close, replace and remove of a fact segment, and
  one-frame supersession.
- **Allowed files.** `src/memory/facts.rs`, `src/memory/facts/tests.rs`.
- **Private surface (proposed).** `MemoryMut::correct_facts(&CorrectionRequest)`.
- **Contract.** Close at c requires `a < c < b`; c at the finite end is an
  exact no-op that still writes a receipt (decision 17). Supersession closes
  and creates or appends in one batch with both series revisions and the slot
  revision guarded. A changed destination is a new series created with its
  full series (`gap4-temporal-graph.md:501`). Remove is not privacy erasure.
- **Tests.** Every boundary; stale series or slot revision; atomic failure of
  the second operation; exclusive slot during supersession; replay.
- **Fail-first.** Omitting the slot guard during supersession fails the race.
- **Oracle.** Fact oracle, correction cases.
- **Size.** M.

### F3: fact query and history

- **Goal.** Read one exact slot at fixed T and C, or its retained history.
- **Allowed files.** `src/memory/facts.rs`, `src/memory/facts/tests.rs`.
- **Private surface (proposed).** `MemoryRef::find_facts(&FactQuery)`.
- **Contract.** Uses `GraphRef::as_of` and `timeline`; `history` forbids an
  explicit T; limit 20/100 with no paging (decision 18); exhaustion follows
  `allow_partial`; reports T, C and view sequence; never reconstructs past
  beliefs.
- **Tests.** T at segment edges; C at expiry; history ordering; limits; budget.
- **Oracle.** Fact oracle, query cases; reuse of `tests/temporal_read_oracle.py`
  read cases is optional, unchanged.
- **Size.** M.

### F4: disputed assertions and contradiction labels

- **Goal.** Opt-in `disputed` disagreement policy and structural labels.
- **Allowed files.** `src/memory/facts.rs`, `src/memory/facts/tests.rs`.
- **Contract.** Contradiction is structural for exclusive slots only; disputed
  values are stored and labelled, never selected as truth; only explicit
  correction resolves them.
- **Tests.** Disputed versus reject; labels at T; resolution by correction.
- **Oracle.** Fact oracle, dispute cases.
- **Size.** S.

### F5: recall over fact segments

- **Goal.** Recall can include fact and relation kinds valid at T.
- **Allowed files.** `src/memory/recall.rs`, `src/memory/facts.rs`, new tests
  in `src/memory/facts/tests.rs`.
- **Contract.** Revisits mandatory transit filtering for the new kinds
  (`gap1-agent-memory.md:298`); scope, TTL and validity apply before
  discovery; ranking formula and existing goldens unchanged.
- **Tests.** Mixed-kind transit isolation; fact at T versus superseded; ranking
  replay unchanged.
- **Oracle.** Ranking oracle unchanged; fact oracle recall cases.
- **Size.** M.

### F6: forget a fact series; entity forget with temporal edges

- **Goal.** Delete a whole series and its history in one receipt frame, and
  make small entity forget and E3 handle temporal incident edges.
- **Allowed files.** `src/memory/facts.rs`, `src/memory/facts/tests.rs`;
  `src/memory/erasure.rs` if E3 has landed.
- **Contract.** Series deletion uses `DeleteEdge` with its revision and the
  slot guard; closing validity is never used as deletion.
- **Tests.** Series history gone after reopen; slot list updated; evidence
  rules as A2.
- **Oracle.** Fact oracle, deletion cases.
- **Size.** M.

## 7. Proposed open decisions (none approved)

1. Name this remainder "C2" and use IDs S1-S2, X1, E1-E4 and F1-F6.
2. Staleness is derived at read time from retained edges, revisions and TTL;
   no write-on-discovery and no persisted stale flag.
3. Staleness work is capped at the summary's 256 edges and, in recall, charged
   to the recall graph budget; no cascade through summaries of summaries.
4. Barrier and job state are one keyed graph job node per scope or entity;
   the scope node itself is not mutated.
5. A completed job keeps its barrier (no reactivation); receipts committed
   before the barrier still replay; jobs and barriers carry no graph TTL.
6. Fencing is the job node's `Expected::Revision` plus a generation in the
   same frame; no KV claim and no dependency on the Gap 2 ledger.
7. A chunk is at most one frame, including progress, manifest delta and
   receipt; order is canonical memory-key order; the token binds scope, job,
   generation, revision and next key, at most 4,096 bytes.
8. `remaining` is exact only when counted within the chunk's bounded scan,
   otherwise `None`.
9. New operations use C1-style length-delimited binary digests under new
   domain strings and new operation names.
10. Revision-changing maintenance requires explicit private reconciliation
    that advances the generation; directory checkpoints do not.
11. Manifests are graph nodes holding target identities and policy only, with
    an explicit retention input; their cleanup is bounded and separate.
12. Entity erasure deletes incident REFERENCES, DERIVED_FROM and content edges
    and leaves dependent summaries S1-stale rather than deleting them.
13. Defer X1; if pursued, it needs a v2 schema that records sources deleted by
    the consolidation itself, plus a new freeze.
14. Slot identity, mode and a bounded series list live on one slot node; the
    mode is immutable; the series-list cap is a number to be chosen.
15. Fact series are keyed temporal edges between entity anchors, with a fixed
    relationship name and a key derived from slot and destination; versioned
    evidence uses new records referenced from segment properties.
16. Add crate-private `MemoryKind::Fact` and `MemoryKind::Relation`; recall
    keeps refusing them until F5.
17. An exact-end close commits a receipt but no temporal entry.
18. Fact query limits 20/100 with no paging; `history` forbids explicit T.
19. New crate-private `MemoryError` variants: `DeletionPending`,
    `JobConflict`, `ReconciliationRequired`, `SlotConflict`,
    `SlotModeConflict`; no public `Error` change.
20. Record F1-F6, when accepted, as delivering Gap 4 slice D; the coordinator
    owns that document update.
21. Independent oracles are required for the E and F tracks and X1, not for
    S1/S2.
22. Track priority: S1, then E1-E2, then F1-F3, then the rest; or another order
    the owner selects.
23. The stale "not implemented" C1 sentence at `gap1-agent-memory.md:314-316`
    is corrected by the coordinator in a separate documentation change.

## 8. Questions for the owner

1. Is a permanent scope barrier after erasure acceptable, or must a scope be
   reusable after a completed job (decision 5)?
2. Should erasure stay strictly single-process under `&mut Database`, leaving
   multi-worker claims entirely to serving (decision 6)?
3. Should X1 be dropped from C rather than deferred (decision 13)?
4. Which track should start first (decision 22)?

No sub-slice is complete, approved, frozen or active because of this document.

## 9. Coordinator rulings (provisional, 2026-11-04)

The coordinator ruled on the open items below using best judgement that is long-term and
backward compatible, while the owner is unavailable. Every ruling is PROVISIONAL, is
logged in the decision ledger, and is revisited at the release go/no-go; none changes a
release gate or the approved 0.6.2 scope.

| Item | Ruling |
|---|---|
| Decisions 1-3, 4-12, 17, 19, 21-23 | Adopted as drafted |
| Decisions 14-16, 18, 20 | Held with Gap 4 slice D: the fact model needs the owner's decisions, so F1-F6 are not authorized |
| Question 1 (decision 5) | A completed erasure keeps its barrier permanently. A later explicit reactivation operation would be additive; a reusable scope could not later be made permanent without breaking callers |
| Question 2 (decision 6) | Erasure stays single-process under `&mut Database`; the job revision and generation guard every chunk, and multi-worker claims are left to serving |
| Question 3 (decision 13) | X1 is not dropped. The memory serving contract offers `logical_delete` as an input disposition (`docs/projects/roadmap/serving-memory-contract.md:338-339`, `:675`), so X1 stays in slice C, after the first S and E sub-slices and before Gap 1 slice D; its v2 schema and freeze are authored separately |
| Question 4 (decision 22) | S1 starts now. The E1 contract and its independent oracle are written in parallel (documentation and Python only); E1 code starts after the oracle is accepted. F-track and X1 wait for their holds and freezes |

Authorized now: S1 (implementation) and the E1 frozen contract with its independent
oracle (no Rust). Every other sub-slice still needs its own authorization.
