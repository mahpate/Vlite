STATUS: DRAFT, paused 2026-11-06; unreviewed; NOT ratifiable; not done: byte-exact manifest and v3 job schemas with numbered decoder rules, the orphan sweep discovery design (open), digest and receipt byte vectors, oracle case I/O tables, a re-verification of every file:line after the owner's next commit (the working tree already differs from `60c9f768` in `src/memory/erasure.rs` +1 line at 31 and `src/memory/erasure/resume.rs` +77 lines at 375, so resume.rs anchors at or after line 375 shift by 77), the 0.7.1 schema-code check for the scope index, receipt-lookup operations for `reconcile_erasure` and the E4 begin variant (section 12).

# Gap 1 E4: reconciliation, deletion manifests, orphan sweep, receipt cleanup and scope index (draft)

Status: **DRAFT. Nothing here is approved, frozen, implemented or activated.** Every shape
that an approved document does not contain is a PROPOSAL with options and a
recommendation (section 10); every file-format version change would be a DECISION for the
coordinator, and none is proposed. Documentation only.
Base: `file:line` anchors were read at `60c9f768` (E2 code committed locally) and the tag
`v0.7.1`. Authority: [C2 remainder](gap1-c2-remainder-contract.md) section 6 E4 and
decisions 10, 11 (with the E2 R13 erratum under decision 10);
[E1](gap1-e1-erasure-barrier-contract.md); [E2](gap1-e2-resume-contract.md) (the v2 job
schema is frozen and is evolved, never changed); [serving contract](serving-memory-contract.md)
sections 3, 5, 6, 8; [Gap 1](gap1-agent-memory.md) sections 5-6. Companion draft:
[E3](gap1-e3-entity-erasure-contract.md).

## 1. Boundary and non-changes

E4 is crate-private and inactive. No public Rust API, `Error` variant, wire shape, header
flag, WAL operation, snapshot encoding, Cargo or dependency change. One new crate-private
`MemoryError` variant is proposed: `ReconciliationRequired`. A file with no erasure data
behaves exactly as before. E4 does not change A1, A2, C1, the E1 begin frame, the E2 v2 job
schema, the E2 token bytes or any E1/E2 golden. Not secure erasure; no backup, replica,
feed or restore guarantee.

## 2. Base facts

| Fact | Evidence (verified) | Consequence |
|---|---|---|
| The database lineage is a nonzero random u64 in the v4/v5 header | `src/format.rs:433` (field), `:1479` (zero lineage is corrupt); public `Database::lineage()`, `src/lib.rs:1666-1667` | A job can record it and compare with `self.db.lineage()` |
| Compaction, upgrade, restore, PITR and re-encryption assign a new lineage | `src/maintenance.rs:509` (compact), `:555` (upgrade), `:411` (restore, fresh lineage "so a restored copy never shares an identity"); `src/logops.rs:47` (reencrypt), `:167-168` (PITR) | Lineage detects those events |
| A directory checkpoint and file-to-directory conversion keep the lineage and restate graph entities at the checkpoint sequence | `src/maintenance.rs:219` and its doc comment ("The lineage is kept"); `maintenance.rs:940` (`write_v5_image` doc: graph snapshot frames at the floor); `docs/projects/roadmap/p2-directory-store.md:137` | Lineage does **not** detect a checkpoint; graph revisions still relabel (E2 R13) |
| Compaction renumbers revisions per body; re-encryption and PITR keep frame sequences | `src/maintenance.rs:940` doc ("without a floor, sequences restart at one"); `src/logops.rs:25` (re-encryption keeps "all sequences ... and revisions"), `:99` (PITR copies frames byte for byte up to the target) | Only compaction, upgrade and checkpoint relabel revisions; restore and re-encryption change lineage only |
| PITR cannot target below a checkpoint floor | `src/logops.rs:119` | Restore points exist only at or above the floor |
| Replica promotion keeps the lineage and loses unreplicated frames | `src/replica.rs:198`, module doc `replica.rs:1-8` (byte-identical prefix, same lineage) | A lineage anchor cannot detect a promotion |
| The pre-barrier replay exception compares receipt and job graph revisions | `src/memory.rs:414` (remember), `:605` (forget); the consolidate read of the barrier `src/memory/consolidate.rs:525` | After a relabel it answers `DeletionPending`, never a wrong replay of a post-barrier write, except after a 0.7.1 rollback (section 9) |
| A v1 job's begin replay requires receipt revision equal to job revision | `src/memory/erasure.rs:387` | A relabel can turn a legitimate v1 begin replay into `InvalidBundle` |
| The E2 fence reads the job revision fresh and the token binds scope, binding, job key, generation, revision, chunk count | `src/memory/erasure/resume.rs:127`, `:720` | A stale token fails with `JobConflict` after any relabel or restore. The serving contract says the token binds lineage (section 5); E2's token does not (erratum, notes) |
| Scoped credentials and memory evidence grants bind the lineage | `src/server/credentials.rs:162-165`; `src/server/memory/routes.rs:295` | A lineage change already invalidates grants; reconciliation is a separate matter |
| An operator may load evidence before attaching it | `src/memory/startup.rs:159` (doc of `holds_only_memory`) | An unattached record is not proof of an orphan (section 6) |
| A reclaim is one 8-byte entry; reclaim-only frames are not committed | `src/property_graph/engine/codec.rs:149`, `src/property_graph/mod.rs:1133` | Graph TTL can retire manifests without a per-node cleanup op (R5) |

## 3. What must fail closed, and what ReconciliationRequired protects

Stale artifacts after revision-changing maintenance or restore, one by one:

- **An issued resume token or a held job revision.** Already safe: `resume_erasure` reads the
  job revision fresh and compares it, the generation and the derived token
  (`resume.rs:720`). A relabel changes the revision, so the old token fails; if a relabel
  happens to leave the revision equal, the job state is the same state and the continuation
  is correct. Nothing needs `ReconciliationRequired` for the fence.
- **A resume receipt replay.** Read-only: it returns the stored original view. Safe.
- **A pre-barrier remember, forget or consolidate receipt.** After a checkpoint (all
  revisions equal the checkpoint sequence) or a compaction (arbitrary renumbering) the
  exception can answer `DeletionPending` instead of replaying. Fail closed, confusing,
  not dangerous. After a 0.7.1 rollback a write made behind the barrier could look
  pre-barrier after a renumbering and replay its (content-free) outcome: a minor leak.
- **A v1 begin replay** can become `InvalidBundle` after compaction.
- **A restore, PITR or replica promotion to a point before a job began.** The file holds no
  trace of the erasure: memories return, no barrier exists, no anchor can fire, and a
  manifest stored in the same graph is gone too. The graph cannot detect this.
- **A restore to a point after the job began** is safe for the barrier: the job is
  present, a pending job re-deletes whatever returned, a `logical` job was complete at that
  point.

Conclusion (PROPOSAL, flagged because it contradicts C2 decision 10's premise):
`ReconciliationRequired` is **unnecessary** for the fence and **insufficient** for
resurrection. What it can honestly protect is an operator-visible gate: after a lineage
change no erasure chunk runs against a history the job did not witness until an operator
has reconciled, which advances the generation (invalidating every issued token), re-anchors
the job and repairs the receipt ordering. Resurrection is covered only by a manifest copy
that lives outside the restored file (section 5).

## 4. Lineage anchor, generation and `reconcile_erasure` (PROPOSAL)

**Schema evolution.** The frozen v2 job (`erase-scope:v2`, `erasure.rs:23`, rules DJ1-DJ6)
is never changed. E4 adds **job schema v3 at the same key** (`JobKey`), replacing v2 by
`UpsertNode`, with the v2 properties plus `e4_lineage` (16 lowercase hex, nonzero) and a
`generation` that may exceed 1 (canonical nonzero u64). `e2_chunks` may be `0` in v3 (no
chunk yet). A **same-key replacement** is required: 0.7.1 honours the barrier by a
presence probe of exactly this key (`erasure.rs:425`; sites in the E3 draft section 2), so
a new key would lift the barrier under 0.7.1. A **sibling anchor node** was rejected for
the scope job because the pre-barrier receipt repair (below) must rewrite the job anyway.
`erase-entity:v1` (E3) carries the same two fields from the start. The decoder dispatcher
(`erasure.rs:301`) gains a v3 arm; v1 and v2 decode as today.

**Who writes v3.** E1 begin and the first E2 chunk keep writing v1 and v2 byte for byte (their
goldens stay valid). v3 is written by `reconcile_erasure` and by E4's begin variant
(below). An `resume_erasure` on a v3 job preserves v3. A v1 or v2 job has no anchor: E2
behaviour, never `ReconciliationRequired`, until reconciled. This leaves a small window
between an E1 begin and its first reconcile; R3 offers the closing variants.

**Gate.** On a v3 or entity job, `resume_erasure` answers `ReconciliationRequired` when
`e4_lineage` differs from `self.db.lineage()`. Precedence: after the job decode and the
live-receipt replay (replay stays allowed), before the `logical` check, deadline and fence.

**`reconcile_erasure(&ReconcileRequest) -> MemoryResult<ReconcileResult>`** (crate-private,
one frame, receipt operation `reconcile-erasure`, its own digest domain
`vlite.memory.erasure.reconcile.v1\0`): validates scope, key, clock, window; reads the job
strictly; upserts it as v3 with `e4_lineage` = current lineage, `generation + 1` (checked)
and a **fresh job revision** (the frame's sequence, above every existing receipt, which
repairs the pre-barrier replay ordering and a v1 begin replay). It deletes nothing, changes
no memory and works in any job state (also `logical`). A retry with the same key replays.
Allowed while reconciliation is pending: `erasure_job`/`erasure_status`, replays,
`reconcile_erasure`, every barrier effect (the barrier never lapses), begin for other
scopes and entities. Refused: new resume chunks of that job.

## 5. Deletion manifests (PROPOSAL)

A manifest is a **content-free durable record of an erasure request**, not of the rows it
removed: target kind (`scope` or `entity`), the scope tuple (hex of `ScopeBytes`), the
entity id, mode `logical_delete`, the job key, `created_at_ms`, `retain_until_ms` and the
creation lineage. No content, vector, source URI, provenance or per-memory list (a scope or
entity target is reapplied by beginning the same job again). Caveat to state to users: an
entity id or scope tuple can itself identify a person, so retention is a privacy input.
Node: label `VliteMemoryErasureManifest` is avoided; E4 proposes label
`VliteMemoryErasureJob` with schema `erase-manifest:v1` under `R("erase-manifest", target)`
(existing reserved label and prefix). `manifest_id` is `mf:v1:` plus the hex SHA-256 of the
domain `vlite.memory.erasure.manifest.v1\0` over `SCOPE || target || begin_digest`, at most
256 bytes. It is written in the **begin frame** (E3 and E4's begin variant), so no chunk
frame and no E2 budget changes; chunk frames never touch it.

**Retention and cleanup.** Retention is a required positive input at begin (`retain_ms`,
checked add, a documented maximum); the manifest carries graph TTL = `retain_until_ms`, so
the engine's reclaim (one entry, `codec.rs:149`) retires it; an explicit bounded sweep is
the alternative (R5). The job, its receipts and the marker never carry TTL.

**Restore.** The manifest lives in the graph, so a restore returns the manifests of the
restore point only. What an operator can do: export manifests before an incident
(`erasure_manifests(page)` read accessor, bounded pages, content-free), and after a
restore call `reapply_manifest`, which begins any job whose target has no job in the
restored file (idempotent by the job keys) and reports targets already barred. What it
cannot promise: it cannot recover an erasure whose manifest was never exported and whose
frames the restore removed; it is not secure erasure; it says nothing about backups,
replicas, logs, feeds or consumers. `erasure_notice.restore_requires_manifest` stays true
(serving contract section 5). Source of truth for reapply is an **exported** manifest value,
verified by its `manifest_id` recomputation (no authentication against coordinated tampering).

## 6. Orphan sweep (R15): status OPEN

Residues (corrected finding): on served data the evidence record of a TTL-reclaimed memory
(reclaim detaches it; a memory's expiry does not propagate to its record) and an
evidence record that an A2 forget detached but retained (`src/memory.rs:1742`); at core
level only, content nodes of reclaimed memories above 4,096 bytes (no TTL; no served route
writes them). **Discovery is the unsolved part.** After the memory node is gone nothing in
the graph names the record: the memory's `evidence` property is gone, receipts hold memory
ids not evidence ids, and an unattached record may be an operator's pre-loaded evidence
(`startup.rs:159`), so "unattached" cannot mean "orphan". Proposal (not verified against a
collection-iteration API): an explicit **plan then apply** over the binding's evidence
collection with a **caller-held cursor** (no full scan in one call, no stored state);
the plan lists unattached record ids in a page; apply deletes only ids that are still
unattached, were listed by a plan at least `min_age_ms` earlier, and for which the caller
passed an explicit acknowledgement that unattached records of this binding are orphans.
Frames: at most 2,000 records per frame (one `Vector::Delete` each, ownership re-checked
with `memory_attachment_owner`), idempotent (an absent id is a no-op), never touching an
attached record. TTL interplay: attached records have no expiry (`GraphAttachedRecordExpiry`,
`src/lib.rs`); a swept record that was due is simply gone. Receipts: the sweep writes a
count-only receipt. Alternatives: a reserved metadata attribution written by a future
remember adapter; an operator-supplied candidate list. Core-level edgeless content nodes
are found by their deterministic keys only when the memory id is known, so they remain
residue unless a plan lists them by a bounded `VliteMemoryContent` label scan with a
caller-held cursor.

## 7. Receipt cleanup (PROPOSAL)

`sweep_erasure_receipts(cursor, now_ms, limit) -> (deleted, next_cursor)`: pages label
`VliteMemoryReceipt` with a caller-held cursor, decodes only receipts whose `e1_schema` is an
erasure receipt schema (`erase-scope:v1`, `erase-chunk:v1`, `erase-entity-begin:v1`,
`erase-entity-chunk:v1`, `reconcile`), and deletes the **expired** ones together with their
outcome nodes in one atomic frame (never an outcome without its receipt, DR6; never a receipt
without its outcome, DR4), at most 512 receipts per frame. E1 and E2 never require an expired
receipt to exist (E2 step 6 ignores it; begin answers `JobConflict` with a job).
Remember, forget and consolidate receipts are not touched. Cost: one label scan cycle per
full pass, bounded per call.

## 8. Scope index or equivalent (DECISION flag for the coordinator)

Finding: E3 entity chunks need **no** scope scan (one node and its adjacency). E2 scope
chunks (`resume.rs:493`), remember and forget (`src/memory.rs:1375`) and recall
(`src/memory/recall.rs:403`) all scan label `VliteMemory` with a `scope` condition, so a
small scope in a large graph pays for every memory node. The existing engine offers a
**declared exact property index**: `GraphMut::put_schema` (`src/property_graph/mod.rs:609`)
stores a `PutSchema` entry (tag 12, `src/property_graph/engine/schema.rs:3`); a declared
exact index on `VliteMemory.scope` makes `find_nodes` iterate that scope's postings in
ascending node ID order (`src/property_graph/engine/query.rs:1115` and its doc comment), the
same order E2 relies on, with results equal to a scan. Format consequence: **none
beyond an ordinary graph schema write** (an existing WAL entry and snapshot encoding), so it
is not a file-format version change by itself. Caveats (all to be verified before a
decision): `PutSchema` replaces the whole schema, so a graph with an operator schema needs a
merge; at most 32 indexed properties per graph (`engine/limits.rs:55`); postings are derived
and rebuilt on schema change and replay; the cost of declaring the index on a large existing
graph in one frame is not measured or verified; whether the 0.7.1 engine accepts and honours
the entry is not verified here. Options: S1 declare the index in a one-time maintenance step
(recommended for evaluation), S2 a per-scope member list node (a format-adjacent change to
A1 remember, rejected), S3 do nothing (accept the O(all memory nodes) per chunk and the
`OperationLimit` ceiling). This is a coordinator decision, not a choice made here.

## 9. Rollback and maintenance

v3 jobs sit at the scope job key and keep 0.7.1's presence probe valid. 0.7.1 does not decode
v2 or v3 jobs and has no route to read them; it honours the scope barrier; it ignores
entity jobs, markers, manifests and anchors. A 0.7.1 write behind a barrier makes post-barrier
receipts that a later relabel can make look pre-barrier (section 3). Manifests and markers
use label `VliteMemoryErasureJob` and prefix `receipt:v1:`, so 0.7.1's reserved-name guard
and memory-only adoption check stay valid (`src/memory/names.rs:15`, `:21`;
`src/memory/startup.rs:159`). Maintenance: checkpoint, compaction and upgrade are treated
alike (graph revisions relabel); lineage-changing events additionally trigger the gate for
v3 and entity jobs.

## 10. Ruling table (all PROPOSAL, status: open)

| # | Question and options | Recommendation |
|---|---|---|
| E4-R1 | Is `ReconciliationRequired` a fence (C2 decision 10) or an operator gate | An operator gate; the fence is already safe (section 3) |
| E4-R2 | Anchor placement: v3 same-key (a), sibling per job (b), graph-level node (c) | (a); (b) and (c) cannot repair receipt ordering |
| E4-R3 | Who writes v3: only `reconcile_erasure` (a), plus an E4 begin variant in one frame with the manifest (b), amend E1 and E2 frames (c, changes frozen goldens) | (b); (c) is a coordinator decision |
| E4-R4 | Unanchored v1/v2 jobs: legacy behaviour (a) or `ReconciliationRequired` (b) | (a); no released route creates jobs |
| E4-R5 | Manifest retention: graph TTL (a) or explicit sweep (b); required input, maximum | (a) with a documented maximum; coordinator sets the policy |
| E4-R6 | Reapply source: exported manifest value only (a) or also in-file manifests (b) | (b), exported value authoritative |
| E4-R7 | Manifest for a small A2 forget: second frame after the forget (a), none (b), amend A2 (c) | (a), disclosed as not atomic |
| E4-R8 | Orphan sweep model: plan then apply with acknowledgement and age (a), reserved attribution (b), operator list (c) | (a); the discovery problem is open |
| E4-R9 | Receipt cleanup: caller-held cursor sweep of erasure receipts only | Adopt |
| E4-R10 | Scope index: S1, S2, S3 of section 8 | Coordinator decision; evaluate S1 |
| E4-R11 | Pre-barrier replay after a relabel: fail closed until reconcile re-stamps the job | Adopt, pinned by the checkpoint golden |

## 11. Acceptance and test plan

Independent oracle (names only): `tests/memory_erasure_reconcile_oracle.py`, fixture
`tests/fixtures/agent_memory/erasure_reconcile_golden.json`, normal and `-O`. Named cases:
`reconcile-v3-job-vector`, `reconcile-v3-decoder-<rule>`, `reconcile-lineage-mismatch-refuses-resume`,
`reconcile-replay-allowed-while-pending`, `reconcile-advances-generation-and-invalidates-token`,
`reconcile-restamps-job-above-receipts`, **`checkpoint-relabel-pre-barrier-remember-receipt`**
(a remember receipt that predates the barrier answers `DeletionPending` after a checkpoint
and reopen, and replays again after `reconcile_erasure`), `compaction-relabel-pre-barrier-forget-receipt`,
`upgrade-relabel-v1-begin-replay`, `restore-before-begin-no-trace`, `restore-after-begin-job-continues`,
`promotion-keeps-lineage-no-gate`, `reconcile-v1-to-v3`, `reconcile-logical-job`,
`reconcile-idempotent-retry`, `manifest-vector`, `manifest-content-free-allowlist`,
`manifest-retention-ttl`, `manifest-reapply-skips-barred`, `manifest-reapply-begins-missing`,
`manifest-export-pages`, `manifest-id-recompute`, `sweep-receipts-expired-only`,
`sweep-receipts-atomic-pair`, `sweep-receipts-cursor-bounds`, `sweep-orphans-plan-apply`,
`sweep-orphans-attached-never`, `sweep-orphans-age-and-ack`, `scope-index-find-order`.
Rust replays on four backends and, for maintenance, through a real `Database::checkpoint()` then
reopen and `maintenance::compact`/`upgrade`/`restore`/`logops::restore_to` where they apply, with
encrypted variants.

Hand mutants with intended killing tests: 1 ignore `e4_lineage` in the gate
(`reconcile_lineage_mismatch_refuses_resume`); 2 gate before the replay
(`reconcile_replay_allowed_while_pending`); 3 do not advance the generation
(`reconcile_invalidates_old_token`); 4 reuse the old job revision (checkpoint pre-barrier
golden); 5 reconcile deletes a memory; 6 reconcile in the wrong state; 7 allow resume on
mismatch for v3 only; 8 apply the gate to v1/v2 (breaks the E2 goldens); 9 put content in the
manifest (`manifest_content_free_allowlist`); 10 chunk frame touches the manifest (E2 budget
goldens); 11 manifest without retention; 12 reapply replaces an existing job; 13 receipt sweep
deletes an unexpired receipt; 14 deletes a receipt without its outcome; 15 sweep deletes
a remember receipt; 16 orphan apply deletes an attached record; 17 apply ignores the age or the
acknowledgement; 18 index changes node order.

## 12. Inputs the serving adapter needs (not a wire shape)

For `erasure_notice.manifest_id` and `memory_job_reconciliation_required`: begin takes
`retain_ms`; every job read returns `manifest_id` and a reconciliation state (`current`,
`required`, `unanchored`); resume returns `ReconciliationRequired`; an operator surface
(CLI or startup step, not in the serving route inventory) calls `reconcile_erasure`;
`erasure_manifests` and `reapply_manifest` for export and restore; the sweeps take a cursor
and return counts. The serving contract has no reconcile route; that gap is open.

Receipt lookup (open, not reconciled): the owner's uncommitted working tree on 2026-11-06
(not part of `60c9f768`, so not a base fact) adds `ReceiptOperation::BeginErasure` and
`ReceiptOperation::ResumeErasure` (`src/memory/mapping.rs`), the matching `ReceiptOutcome`
variants (`src/memory/receipt.rs`) and `lookup_begin` and `lookup_chunk`
(`src/memory/erasure/resume.rs`). This draft does not give `reconcile_erasure`, the E4 begin
variant or the sweeps a receipt-lookup operation. PROPOSAL, unverified against the
uncommitted code: `reconcile_erasure` and the E4 begin variant add their own operation
names (their receipts differ in schema from E1 and E2); the sweeps return counts and need
no lookup. The coordinator or the owner decides when the ES slice lands.

## 13. Not frozen

Everything above: R1-R11, the v3 and manifest schemas, digests, the sweeps, the index
choice, the case and mutant lists, serving, X1, F-track items and any secure-erasure,
backup, replica or feed claim.
