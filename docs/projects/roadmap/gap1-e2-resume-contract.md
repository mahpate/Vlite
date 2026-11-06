# Gap 1 E2: fenced resume chunks for the exact-scope erasure job (proposed)

Status: **PROVISIONALLY RATIFIED (R1-R17, 2026-11-06, coordinator under owner
delegation); ratification is not a release go.** The persisted schemas of
sections 3-4 are persisted graph state and are frozen by it; they stay flagged
for the release go/no-go like E1's. Implementation and the independent oracle
(section 9) are in progress. Nothing here is activated, public, wired to a
route or a format change.
Base: `b4673670` (origin/main, v0.7.1 plus test-only commits).
Authority: [C2 remainder](gap1-c2-remainder-contract.md) section 6 E2,
decisions 4-12 and section 9, and the ratified
[E1 contract](gap1-e1-erasure-barrier-contract.md) (jobs, barrier, begin).
Subordinate to [Gap 1 section 5](gap1-agent-memory.md#5-forget-retention-and-erasure)
and to the [memory serving contract](serving-memory-contract.md) section 5,
whose wire it must satisfy later without changing it. "Exact" below specifies a
proposed private semantic version, not a new engine encoding, public API,
serving schema or on-disk format: memory state stays ordinary graph nodes,
edges, attachments and receipts.

## 1. Boundary, identity and amendments to E1

E2 adds one operation, `resume_erasure`: it deletes one bounded, deterministic
chunk of the live memories of the exact scope of an existing job, and advances
that job in the same graph frame. When no live memory remains, the same frame
moves the job to `logical`. Nothing else changes:

- No new barrier, no new target. Entity, session-versus-scope variants, fact
  series and edge-level chunking are E3/F6; manifests, reconciliation after
  revision-changing maintenance, lineage fencing and residue cleanup are E4.
- A chunk deletes whole memories only. A memory whose own deletion plan does
  not fit one frame stops the job with `OperationLimit`, changes nothing and
  is not skipped (E3 adds edge-level chunks for such hubs).
- No wire, REST, gRPC, MCP, CLI, client, OpenAPI or proto change; no public
  `Error` or Rust API change; no header flag, WAL operation or snapshot
  encoding; no Cargo change; no dependency on KV, leases or the Gap 2 ledger.
- Single process: every step runs inside one call under `&mut Database`
  (C2 decision 6). The job revision and generation fence chunks; there is no
  worker, timer, scheduler or retry loop.
- Not secure erasure. Receipts, outcomes, the scope node and the job stay;
  memories already hidden by TTL are not candidates and leave residue
  (section 5); backups, replicas, logs and feeds keep what they held.

Amendments to E1 (the E1 Rust tests, oracle and golden stay valid unchanged,
because every E1 state is a schema-v1 job):

| E1 item | E2 amendment |
|---|---|
| Job schema | Schema `erase-scope:v1` is read as before. `erase-scope:v2` is the progressed form (section 4); a v1 job becomes v2 in the frame of its first chunk. E1's probes that set an unknown schema on a v1-shaped node still fail: a node must carry the exact property set of its schema |
| Helpers | `begin_bytes`, `begin_digest`, `begin_receipt_key` and `job_key` keep their signatures and bytes: E1's tests import them |
| `erasure_status` | Unchanged and v1-only (E1's frozen tests pin it). A v2 job answers `InvalidBundle` through it. Serving uses the new `erasure_job` |
| `begin_erasure` | Unchanged on v1 states. On a v2 job it reads the job through the v2 decoder and still returns the replay of a live matching receipt or `JobConflict`. The begin receipt of a v2 job is checked by digest only: E1's equality of the receipt's revision with the job's holds for a v1 job alone (section 7) |
| `ErasureState` | Gains `Logical`; E1's lowercase `Debug` projection (`pending`) is unchanged |
| Resume token | `None` stays pinned for E1's `erasure_status`. `erasure_job` returns the token of any pending job, including one begun by E1 code |

## 2. Private types, validation and order

```text
ErasureJobRequest = ErasureStatusRequest                 // scope only
ErasureJob {
  job_key: String, revision: u64, generation: u64,
  state: Pending | Logical, logical_visibility: Hidden,
  deleted_count: u64, remaining: Option<u64>, chunks: u64,
  resume_token: Option<String>                           // Some iff Pending
}
ResumeErasureRequest {
  scope: MemoryScope, job_key: String, idempotency_key: String,
  expected_revision: u64, generation: u64, resume_token: String,
  now_ms: u64, receipt_window_ms: u64, deadline_ms: Option<u64>
}
ChunkSummary { memories_deleted: u64, remaining_after: Option<u64> }
ResumeErasureResult {
  job: ErasureJob, chunk: ChunkSummary,
  receipt_revision: u64, commit_sequence: u64, replayed: bool
}
```

New crate-private surface: `MemoryRef::erasure_job(&ErasureJobRequest) ->
MemoryResult<ErasureJob>` (strict, read-only, barrier-exempt) and
`MemoryMut::resume_erasure(&ResumeErasureRequest) -> MemoryResult<ResumeErasureResult>`.
No new `MemoryError` variant: `JobConflict` is every fence failure (E1 section 2
already reserves it), `MissingMemory` is a missing job or a job key that is not
this scope's, and the existing validation, receipt, limit and storage errors
apply. `ForeignAttachment` and `InvalidEvidence` are A2's.

Structural validation, in this order, before any read: scope; opaque
`idempotency_key` (1..256 UTF-8 bytes, no controls); `now_ms` in `0..=i64::MAX`
(`InvalidClock`); positive `receipt_window_ms` with checked `now + window`
(`InvalidReceiptWindow`); nonzero `expected_revision` and `generation`
(`InvalidTarget`); `job_key` opaque (`InvalidKey`); `resume_token` shaped
`t1:` followed by 64 lowercase hex digits (`InvalidTarget`; a wrong value of
the right shape is a fence failure, not a shape failure); a present collection
name of 1..63 UTF-8 bytes (E1 section 2, `InvalidBundle`).

Then, strictly in this order:

1. Read the provisioned scope node and verify its tuple (`InvalidScope`,
   `DigestCollision`), exactly as E1 begin does.
2. Probe the singleton job key. If present, decode it strictly (v1 or v2;
   `InvalidBundle`, `DigestCollision`).
3. Probe the resume receipt of the request key. If present, decode it
   strictly, live or expired (section 4). A receipt with no v2 job (the job is
   absent, or still v1) is `InvalidBundle`: E1 answers a receipt without its
   job the same way. If it is absent while an outcome node of its key exists
   (index 0 or 1), that is `InvalidBundle` too (DR6).
4. No job: `MissingMemory`.
5. `job_key` of the request differs from the scope's `JobKey`: `MissingMemory`.
6. A live receipt (`now < expiry`): the same digest replays (section 6)
   before the state, deadline and fence are consulted; another digest is
   `IdempotencyConflict`. An expired receipt is ignored.
7. A job in state `logical`: `JobConflict` (no further chunk exists).
8. `deadline_ms` present and `now_ms >= deadline`: `DeadlineExceeded`.
9. Fence: `generation` equals the job's, `expected_revision` equals the job's
   current graph revision, and `resume_token` equals the token recomputed from
   the job (section 3). Any mismatch is `JobConflict`, and nothing is written.
10. Plan the chunk (section 5). Every planning failure changes nothing.
11. Write one graph frame (section 5). A frame error is returned as A2 returns
    it, as the engine's `Storage` error, and is never converted to
    `JobConflict`: under `&mut Database` nothing can change the job between
    steps 9 and 11, so a frame guard fails only through the TTL reclaim of
    section 7. `CommitIndeterminate` is preserved and never retried blindly.

`erasure_job` validates only the scope, then runs steps 1-2 and answers
`MissingMemory` for an absent job.

## 3. Exact bytes and keys

Notation, hash and encodings are E1 section 3 (`H`, `S`, `SCOPE`, `O`, `U32`,
`U64`, `R(op,key)`, `ScopeBytes`, `ScopeKey`, `JobKey`, `OutcomeKey`).

```text
chunks(job) = e2_chunks for a v2 job, 0 for a v1 job

TokenBytes = ASCII "vlite.memory.erasure.token.v1\0"
          || SCOPE(ScopeBytes) || O(bound_collection)
          || S(JobKey) || U64(generation) || U64(job_revision) || U64(chunks)
Token = "t1:" + hex(H(TokenBytes))                       (67 ASCII bytes)

ResumeBytes = ASCII "vlite.memory.erasure.resume.v1\0"
           || SCOPE(ScopeBytes) || O(bound_collection)
           || S(JobKey) || U64(expected_revision) || U64(generation)
           || S(resume_token) || U64(receipt_window_ms)
ResumeDigest = "v1:" + hex(H(ResumeBytes))

ResumeReceiptKey = R("resume-erasure", idempotency_key)
OutcomeKey(receipt) = receipt_outcome_key(ResumeReceiptKey, 0)
```

`job_revision` is the actual graph revision of the job node. `now_ms`,
`deadline_ms` and `idempotency_key` are excluded from `ResumeBytes` (the key
still determines the receipt identity). Every request field that selects the
fence, and the window, are hashed. The token is derived, not stored and not
secret: anyone who can read the status can compute it. It prevents a stale or
concurrent resume, not an unauthorized one; authorization is the caller's
(serving's) exact-scope grant.

The resume outcome is exactly seven LF-terminated lines, no other bytes:

```text
erase-chunk:v1
<JobKey>
<chunk_no>
<memories_deleted>
<deleted_total>
<remaining, or the word unknown>
<the word pending, or the word logical>
```

Numbers are canonical unsigned decimals (`0` or no leading zero), `chunk_no`
is at least 1. The text is below 512 bytes (`JobKey` is 149 bytes). The
original revision is read from the actual receipt node after commit or reopen.

## 4. Exact persisted schema and strict decoders

All nodes have exactly one label and exactly the listed properties, no graph
TTL, temporal segment or inline outcome. Strings are at most 4,096 bytes;
integers use the engine i64. u64 counters are canonical decimal Strings: `0`
or no leading zero, as E1 section 4 specifies for counters (`e2_chunks` and
`expires_at_ms` are also nonzero, as E1's expiry). Every bound of the rules
below that multiplies a counter is compared exactly, in 128-bit arithmetic.

**Job, schema v2** (`VliteMemoryErasureJob`, key `JobKey`; replaces the v1
node by `UpsertNode`, which keeps the node's edges and attachments, none of
which E1 or E2 create):

| String properties (exact) | Integer property (exact) |
|---|---|
| `e1_schema`=`erase-scope:v2`, `scope`=ScopeKey, `e1_scope_tuple`, `e1_collection`, `begin_digest`, `generation`=`1`, `state`=`pending` or `logical`, `logical_visibility`=`hidden`, `deleted_count`=u64, `remaining`=`unknown` or u64, `e2_chunks`=nonzero u64 | `created_at_ms` (unchanged from the v1 node) |

The v2 job is the v1 job plus `e2_chunks`. Its decoder is closed; these rules
and no others apply (each is one golden, section 9):

- DJ1. Label, exact property names and types, string bounds and no graph TTL,
  as E1's decoder. Edges, attachments and the node's revision are not
  inspected.
- DJ2. `e1_schema` is `erase-scope:v2`; `scope` is the ScopeKey;
  `e1_scope_tuple` is the hex of the scope bytes (a mismatch is
  `DigestCollision`); `e1_collection` is the handle's binding.
- DJ3. `begin_digest` has the digest shape `v1:` plus 64 lowercase hex digits;
  `generation` is `1` (only E4 reconciliation advances it);
  `logical_visibility` is `hidden`; `created_at_ms` is at least 0.
- DJ4. `state` is `pending` or `logical`; `deleted_count` is a canonical u64;
  `remaining` is `unknown` or a canonical value in `0..=4095`; `e2_chunks` is
  a canonical nonzero u64.
- DJ5. `state` is `logical` if and only if `remaining` is `0`.
- DJ6. `deleted_count` is at most 2,046 times `e2_chunks`.

**Resume receipt** (`VliteMemoryReceipt`, key `ResumeReceiptKey`) has E1's
begin-receipt property set (`e1_schema`, `scope`, `e1_scope_tuple`,
`e1_collection`, `digest`, `expires_at_ms`, `job_key`, `outcome_chunks`) with
`e1_schema`=`erase-chunk:v1`, `digest`=ResumeDigest, `job_key`=JobKey,
`outcome_chunks`=1 and `expires_at_ms`=decimal(`now + receipt_window_ms`). Its
outcome node (`VliteMemoryReceiptOutcome`, key `OutcomeKey`) has `receipt_key`,
`outcome` and `index` as E1's. Strict decoding applies even to an expired
receipt, with these rules and no others:

- DR1. Label, exact property names and types, string bounds and no graph TTL.
- DR2. `e1_schema` is `erase-chunk:v1`; `scope`, the tuple and
  `e1_collection` as DJ2.
- DR3. `digest` has the digest shape; `job_key` is the scope's `JobKey`;
  `outcome_chunks` is 1; `expires_at_ms` is a canonical nonzero u64.
- DR4. The outcome node with index 0 exists and the one with index 1 does not.
- DR5. The scope's job is a valid v2 job (step 3 of section 2).
- DR6. When the receipt is absent, no outcome node of its key exists (index 0
  or 1).
- DO1. The outcome node has exactly the properties above; `receipt_key` is the
  receipt's key; `index` is 0.
- DO2. `outcome` parses as exactly the seven lines of section 3 with canonical
  numbers, and re-rendering the parsed numbers reproduces the stored text.
- DO3. Line 2 is the scope's `JobKey`; `chunk_no` is at least 1;
  `memories_deleted` is at most 2,046; `deleted_total` is at least
  `memories_deleted` and at most 2,046 times `chunk_no`; chunk 1 has
  `deleted_total` equal to `memories_deleted`; `remaining` is `unknown` or in
  `0..=4095`; the state is `logical` if and only if `remaining` is `0`; and
  `memories_deleted` of 0 implies `remaining` of `0` (a chunk that deletes
  nothing exists only when no live memory remained).
- DO4. Against the job: `chunk_no` is at most `e2_chunks`; `deleted_total` is
  at most `deleted_count`; a `logical` outcome has `chunk_no` equal to
  `e2_chunks`; and when `chunk_no` equals `e2_chunks` (the latest chunk) the
  outcome's `deleted_total`, `remaining` text and state equal the job's.
- DO5. Nothing else is checked. In particular no relation between the graph
  revisions of the receipt, outcome, job or begin receipt (section 7), and no
  relation between expiry and a clock.

A tuple mismatch is `DigestCollision`; every other malformed state is
`InvalidBundle`; no forgiving fallback exists.

**v1 to v2.** A v1 job (E1) has `generation=1`, `state=pending`,
`deleted_count=0`, `remaining=unknown` and no chunk. Its first chunk writes the
v2 node with `e2_chunks`=1, the new `deleted_count`, `remaining` and `state`,
and every other property carried over. The begin receipt of a v2 job decodes
with E1's rules except that the equality of its revision with the job's is not
required (its `digest` must still equal the job's `begin_digest`).

## 5. Chunk planning

A chunk is planned from a read at the request clock (`graph.with_clock(now)`),
as A2 reads. Planning changes nothing.

**Window.** The window is the first 4,097 live memories of the exact scope in
ascending engine node-ID order (creation order): `find_nodes` for label
`VliteMemory` with the condition `scope = ScopeKey`, `limit` 1,000, the graph
read ceilings, continued by cursor. Every page asks for 1,000 and the list is
cut at 4,097. A page that ends incomplete without a cursor is `OperationLimit`.
No cursor is stored: each chunk rescans from the lowest node ID, because
deleted memories no longer match. A memory hidden by TTL at `now` is not a
candidate. Node IDs are never reused, and compaction, upgrade and conversion
keep them, so the order is stable; the barrier refuses memory writes, so a
pending scope only shrinks. The memory layer declares no property index, so a
page examines the `VliteMemory` nodes of every scope in ID order: a chunk
examines them up to the page that holds the 4,097th candidate (that page can
return up to 1,000 matches, so up to 5,000 candidates are read before the cut),
or to the last `VliteMemory` node of the graph when fewer candidates exist.

**Decode.** Every candidate of the window is decoded with A2's
`existing_memory`, and must also have exactly the label `VliteMemory` and the
key `memory_key(ScopeKey, id)`. Every `existing_memory` error and every failed
check is `InvalidBundle` (as C1 maps it) and aborts the chunk before any
planning, whatever the candidate's position. If the window
holds fewer than 4,097 candidates (`N` of them) the total is known and the
chunk's `remaining_after` is `N - memories_deleted`; otherwise it is unknown.

**Per-candidate plan.** Candidates are visited in order. For candidate `m`:

1. Evidence, as A2: `m.evidence` empty and any attachment present is
   `ForeignAttachment`. Otherwise the handle must carry an evidence collection
   that resolves (`InvalidEvidence`); every attachment must belong to it
   (`ForeignAttachment`); `m.evidence` must hold exactly one entry
   (`InvalidBundle`); `memory_attachment_owner(collection, record, now)` must
   be `(graph, m.key)` (`ForeignAttachment`); and the attached record must be
   that record (`InvalidBundle`). The plan has `Detach` and `Vector::Delete` of
   the record. **E2 never retains evidence** (R5): no memory of the scope
   remains at completion, so A2's reachability retention would orphan the
   record. A record has one owner (an attachment is exclusive, and
   `memory_attachment_owner` answers at most one `(graph, node)`), so the owner
   check proves that the record is this memory's. A memory that names a record
   owned by a memory of another scope, by an application node or by another
   graph is damage: the job stops with `ForeignAttachment`, and that record is
   neither detached nor deleted.
2. Incident edges, both directions, deduplicated by edge ID, read in pages with
   the graph read ceilings until the list ends. More than 4,096 unique edges
   stop the reading: `m` can never fit, and its content (step 3) is not
   validated.
3. Content. Each incident `HAS_CONTENT` edge whose source is `m` names a
   content node. With `i` the target's `index` property, which must be a
   non-negative Integer, the edge key must be `content_edge_key(m.key, i)`, the
   target key `content_key(m.key, i)`, its labels exactly `VliteMemoryContent`
   and its `scope` the ScopeKey, else `InvalidBundle`. The content node's only
   visible incident edge must be that `HAS_CONTENT` edge (else `InvalidBundle`)
   and it must have no attachment (else `ForeignAttachment`): the node delete
   removes attachments, and an edge would refuse it. Each check is one
   `neighbors` read and one attachment read per content node. Each content node
   is deleted after its edges.
4. The memory node delete with `Expected::Revision` of the revision read.

The evidence check (step 1) always runs before the edge read, and steps 1-3 run
before the fit test, so a candidate that does not fit is still validated (an
error aborts the chunk), except that a candidate with more than 4,096 unique
edges skips step 3.

**Fit rule.** Cost is counted as A2 counts, plus the E2 operations:

```text
ops(chunk)    = 3                              // job upsert, outcome, receipt
              + |evidence ops| + |unique edges| + |content nodes| + |memories|
guards(chunk) = |memories| + 1                 // A2's per-target guards + job
```

`m` joins the chunk when `ops + guards` with `m` added is at most 4,096 (A2's
`MAX_OPS`); an edge shared by two chunk members counts once. The first
candidate that does not fit ends the chunk (no later candidate is tried). If it
is the first candidate, the job stops: `OperationLimit`, nothing written,
nothing skipped. A chunk is empty only when no candidate exists. An encoded
body above 16 MiB is `BodyLimit` from the engine, never truncated. A chunk
holds at most 2,046 memories (each costs an operation and a guard, and four are
fixed), so a scope of more than 2,046 live memories always needs several
chunks.

**Frame.** Operation order is fixed: evidence ops per memory in candidate order
(`Detach` then `Vector::Delete`); `DeleteEdge` for each unique edge in
ascending edge ID with `Expected::Revision`; `DeleteNode` for content nodes in
that edge order; `DeleteNode` for the memories in candidate order; the job
`UpsertNode` with `Expected::Revision(job revision)` and the v2 properties; the
outcome node; the receipt node last. An expired receipt of the same key and its
outcome node are replaced by `UpsertNode` with `Expected::Revision` of the old
node (A2's rule); otherwise they are created with `CreateNode`, which fails
when the key exists.

**Result.** The new job `revision`, the receipt revision and the commit
sequence are all the frame's sequence. `deleted_count` grows by the memories
deleted, `chunks` by one, `remaining` is `remaining_after` (or `unknown`), and
`state` becomes `logical` exactly when `remaining_after` is known to be `0`,
which also makes `remaining` `0`. A scope with no live memory completes in its
first chunk with `memories_deleted = 0`; a later chunk can also complete with
`memories_deleted = 0` when the remaining memories expired between chunks. The
returned token is the token of the new revision when `pending`, absent when
`logical`.

**Stops.** These states stop the job at every chunk until someone repairs them
outside E2, which never skips: a job bound to no collection (E1 allows the
begin through such a handle) over a scope with evidence (`InvalidEvidence`);
evidence attached in another collection, an application's own attachment on a
memory node or on one of its content nodes, or an owner mismatch
(`ForeignAttachment`); an inconsistent evidence list, memory or content
structure, including a content node with another visible edge
(`InvalidBundle`); and a first candidate that alone does not fit
(`OperationLimit`). A handle bound to a
different collection cannot resume the job (`InvalidBundle`, as E1). Repair is
an administrative action through raw graph access, or a later tool; the
user-facing message must say the job is stopped, never that it is complete.

**Residue.** A completed job keeps the job (`logical`), the begin and resume
receipts with their outcomes and the scope node, and the receipts of earlier
remember, forget and consolidate operations, whose outcome nodes name memory
IDs (hex-encoded). It also leaves what E2 cannot reach, to be swept by E4's
bounded orphan cleanup (R15):

- On served data (0.7.1 routes): the evidence records of memories that graph
  TTL reclaimed, before the barrier or by a chunk frame's opening reclaim
  (reclaim removes a due node with its edges and attachments, so the record is
  detached; a memory's expiry does not propagate to its record, which
  expires only if it was given an expiry of its own), and the evidence records
  that an earlier A2 forget detached but retained under its reachability rule.
  A forget that retains nothing deletes its evidence record, so that case
  leaves none.
- Only for data written through the core with content over 4,096 bytes: the
  `VliteMemoryContent` nodes of reclaimed memories (they carry no TTL, and no
  edge reaches them afterwards). Content nodes exist only for such content
  (`MAX_STRING_VALUE_BYTES` and the chunking branch of `remember` in
  `src/memory.rs`), and every served route caps content at 4,096 bytes
  (`MAX_CONTENT_BYTES` in `src/server/memory/routes.rs`), so served data holds
  none. A1 and C1 are not changed by this finding.

Scope erasure removes live memories, not every byte they once referenced.

## 6. Barrier, replay and the E1 matrix

The barrier is unchanged: job presence, checked right after the scope-node
read, still decides every other operation. E2 changes no barrier row, but the
pre-barrier replay exception of the other operations (a receipt older than the
job replays) compares graph revisions: after a checkpoint and reopen the job
and those receipts can carry one revision, and the exception then fails closed
with `DeletionPending` until a chunk raises the job revision, and for good once
the job is `logical` (compaction can do the same to receipts that share the
job's body); E4 reconciles, and the maintenance goldens expect it. A chunk
frame deletes only nodes of memories of this exact scope and their verified
content nodes, and the edges incident to those memories (a content node has no
other edge, section 5); it never touches the scope node, a
receipt of another operation or another scope's memory. Child and sibling
scopes stay unbarred and untouched.

**Replay of a resume.** A live receipt with the same digest returns the result
rendered from its stored outcome: `chunk_no`, `memories_deleted`,
`deleted_total`, `remaining` and `state` from the outcome; `revision`,
`receipt_revision` and `commit_sequence` from the actual receipt node;
`generation` 1; `chunks` equal to `chunk_no`; the token of that revision and
chunk number when `pending`; `replayed = true`. It is the original chunk's
view, not the current job: after later chunks its token is stale and a fresh
`erasure_job` read is the way to resume. Replay needs the valid v2 job
(step 3 of section 2). Replay writes nothing and ignores the deadline.

**Same key, after expiry.** The key may be used for a new chunk: the old
receipt and outcome are replaced in the new frame (section 5). A retry of a
chunk that already committed, made after its receipt expired, therefore fails
the fence (`JobConflict`, or `DeadlineExceeded` first when its deadline also
passed).

**`erasure_job`.** Returns the current job: `job_key`, the node revision,
generation, state, hidden visibility, `deleted_count`, `remaining`, `chunks`
and the token of the current revision when `pending`. A v1 job reports 0
chunks, `deleted_count` 0 and `remaining` `None`.

**`begin_erasure` after the first chunk.** Replays its live matching receipt
(original revision from the receipt, E1 section 5) or answers `JobConflict`
as E1 specifies, now over a v2 job.

## 7. Atomicity, budgets, clocks, maintenance and rollback

One chunk is one graph frame: evidence ops, edge and node deletes, the job
upsert, the outcome and the receipt commit together or not at all. Validation,
planning and pre-append refusal change nothing: the job, the memories, the
receipts and the evidence are untouched. `CommitIndeterminate` after a possible
append is preserved; reopening shows either the whole chunk (its receipt
exists and the job carries its numbers) or none of it. A retry with the same
key and request replays a committed chunk inside its receipt window and
re-plans an uncommitted one.

**Clocks and TTL.** The commit clock is `max(now, wall clock, graph high-water)`,
captured once; no E2 node has a TTL. The frame first reclaims every entity due
at that clock, then applies its operations. Planning saw the entities that
expire after `now` as live, so a memory or edge that expires in
`(now, commit clock]` is already gone when its planned operation runs (the
evidence operations of such a memory fail because its node is gone; attached
records have no expiry of their own): the frame fails with a `Storage` error (a
revision mismatch or a not-found), commits nothing, and E2 does not remap it. A
caller retries with a current clock; a retry at the same `now` plans the same
entity and fails the same way, one at a clock at or after its expiry no longer
sees it. The race needs a commit clock above the request clock: a request clock
behind the wall clock, or a graph high-water above it. Real frames raise the
high-water to the clock they reclaim at, so tests reproduce the race with the
harness-only high-water poke (`graph_clock` set above the request clock before
the call, as the E1 and C1 tests do) and oracle cases model it as `high_water`
in the setup. TTL cases use clocks above the wall clock so that the wall clock
never decides an outcome (E1's begin frame already reclaimed everything due at
its own clock). The frame's reclaim also removes the nodes of memories that were
hidden at `now` (R14), without counting them as deleted.

**Reads.** A chunk examines the `VliteMemory` nodes described in section 5 (up
to 5,000 candidates, or every node of the label when fewer match), plus the
planning reads of its members and of the first candidate that does not fit; no
chunk claims a latency or size figure.

**Reopen.** Acceptance round-trips every v2 node, the token, the replay result,
the fence failures and the barrier matrix on plain file, encrypted file,
directory store and encrypted directory store, with a reopen between every
step, a pre-append failure (nothing changes) and a reopen straight after an
append (the whole chunk is there).

**Maintenance.** An ordinary directory checkpoint restates every graph node and
edge at the checkpoint sequence (only a precision image restores revisions,
`src/maintenance.rs` `write_v5_image`; the p2 design says "Graph entities
restate at S"), and compaction and upgrade renumber them per body.
`Database::checkpoint()` installs the image but keeps the open handle's graph:
the restated revisions appear when the store is next opened, so maintenance
cases checkpoint and reopen. Afterwards
the job and every receipt can carry one shared revision or unrelated ones, so E2
decides nothing from a relation between graph revisions: the fence compares
`expected_revision` with the job node's current revision, the latest chunk
receipt is the one whose `chunk_no` equals `e2_chunks`, and the begin receipt of
a v2 job is checked by digest only (section 4). After such maintenance a token
read earlier may be stale (`JobConflict`; it stays valid only when the job
revision did not change): read the job again with `erasure_job`;
replays of older keys keep working. A v1 job keeps E1's equality of the begin
receipt's revision with the job's, so its begin replay can fail `InvalidBundle`
after compaction or upgrade (E1's documented limit). Barrier and deletion state
are unaffected, except for the pre-barrier replay exception of section 6; E4
owns reconciliation and lineage fencing. A pending job is
never silently completed or skipped by maintenance. This corrects C2 decision 10
and the E4 test list, which say that directory checkpoints do not renumber: for
graph entities of an ordinary store they do, so E4 must treat a checkpoint like
a compaction for graph revisions.

**Rollback.** A build without E1 (0.7.1) ignores the job nodes: the barrier is
not enforced, so a pending scope becomes readable and writable again, and so
does a completed one. After rolling forward again, a `logical` job hides the
memories written meanwhile and refuses every further chunk (R9), so they can
never be erased by the job; a `pending` job would delete them in later chunks.
Do not roll back while any job exists. This is a one-way feature to document at
serving.

## 8. Ruling table (R1-R17 provisionally ratified 2026-11-06, not a release go)

| # | Open point and options | Choice | Rationale |
|---|---|---|---|
| R1 | E1 compatibility: amend E1 fixtures, or add accessors | Add `erasure_job` and `resume_erasure`; E1 functions, helper signatures and every E1 fixture unchanged; v1 becomes v2 at the first chunk | Accepted fixtures are additive-only (C2 section 5); E1 never shipped, so no data migration exists |
| R2 | Chunk order: memory-key order (C2 decision 7 as drafted) or node-ID order | **Ascending node ID.** Amends C2 decision 7 | A memory key is `mem:v1:` plus a hash of the ID, so key order needs a full-scope read and sort per chunk; ID order is creation order, stable across maintenance, and needs no stored cursor. Alternative: key order with a bounded scan and a stuck job beyond it |
| R3 | Remaining count | Exact only when the 4,097 window reaches its end, else `unknown` | C2 decision 8; bounds the candidates a chunk decodes |
| R4 | Oversized member | Stop with `OperationLimit`, nothing skipped | Never skip silently; E3 owns edge-level chunks for hubs |
| R5 | Evidence of a deleted member | Always detach and delete; no retention | A2 retains evidence reachable from memories that remain; in a scope erasure every memory goes, so retention would orphan records forever. A record has one owner, so the owner check of step 1 keeps a record of another scope out of reach (`ForeignAttachment`, never deleted) |
| R6 | Foreign attachment and other stops | Stop with an explicit error, never a skip; section 5 lists them | C2 E2 text |
| R7 | Fence | Generation, job revision and a derived token, checked before planning (step 9); the frame repeats the job and receipt revision guards as defense in depth | C2 decision 6; stale token or concurrent chunk cannot commit. The frame guards cannot fail under `&mut Database` (hand mutants on them are equivalent, section 9) |
| R8 | Token | `t1:` plus a SHA-256 over scope, binding, job key, generation, revision and chunk count; derived, not stored | Stateless and deterministic; staleness and concurrency, not authorization |
| R9 | Resume of a `logical` job | `JobConflict` for a new key; replay of an old key works | The barrier is permanent (C2 decision 5) |
| R10 | Receipt contents | Counts only: no memory IDs, no content | Privacy; the deletion manifest and its retention are E4 |
| R11 | Replay view | The original chunk's numbers and the token of its revision and chunk number | C2 E2: replay returns the original chunk result |
| R12 | Errors | No new `MemoryError` variant; frame errors are returned as A2 returns them | Existing categories suffice (E1 R-table style) |
| R13 | Maintenance | No decision from graph-revision relations; latest chunk by `chunk_no`; document and let E4 reconcile; corrects C2 decision 10 for checkpoints | Checkpoint and compaction renumber graph revisions (section 7) |
| R14 | Hidden expired memories | Not candidates; the frame's own reclaim removes their nodes | They are invisible at `now`; deleting them needs a TTL-aware scan outside E2 |
| R15 | Residue (on served data: evidence records of reclaimed memories and evidence retained earlier by A2; content nodes only for core data over 4,096 bytes, which served routes never write) | Not swept by E2; E4 owns a bounded orphan sweep. Option: sweep the scope's edgeless content nodes in the completing chunk (costs a second label scan and changes `remaining`) | No manifest exists yet to account for it; recommend E4 |
| R16 | Planning strictness | Decode the whole window; check the memory key and label; verify content edges and nodes, and that a content node has no other edge and no attachment (A2 trusts them) | Erasure must not delete a node, or detach a record, it cannot prove is this memory's |
| R17 | Amendments to C2 section 6 E2 | The surface returns `ResumeErasureResult` (not `ErasureStatus`); allowed files add new `src/memory/erasure/resume.rs` and `src/memory/erasure/resume_tests.rs` (hooks in `erasure.rs`, visibility in `src/memory.rs`); the test "shared evidence retained" becomes "evidence never retained"; "same erasure oracle" becomes two new files; E1 section 8's plan to append E2 groups to `erasure_golden.json` is replaced by a separate golden | Accepted fixtures stay additive-only; keeps `erasure.rs` and its tests small |

## 9. Independent acceptance and not frozen

New files only: `tests/memory_erasure_resume_oracle.py` and
`tests/fixtures/agent_memory/erasure_resume_golden.json`. The accepted
`tests/memory_erasure_oracle.py` and `erasure_golden.json` stay unchanged; the
new oracle may import their pure helpers read-only. Goldens pin: the token,
resume digest, receipt, outcome and v2 job byte vectors; every decoder rule of
section 4 (one rejected golden per rule, each declaring one repairable defect)
and the v1 to v2 transition; the order of steps 1-9 of section 2 (each
precedence probe separate, including a receipt beside no job, a wrong job key
beside a live receipt and an old job); the window, the decode of the whole
window and the 4,097 cut; the fit rule at exactly 4,095, 4,096 and 4,097
operations with shared edges counted once, and a hub of more than 4,096 edges;
content edge and node validation, including a content node with another edge
or an attachment, and an outcome node without its receipt (DR6); evidence
ownership, including a record owned by a memory of another scope (the job
stops with `ForeignAttachment` and that record is never deleted), and every
stop of section 5; completion, the empty scope and a chunk that completes empty;
replay of the original view after later chunks; expired-key reuse and the
`JobConflict` of a late retry; key and digest conflicts; begin replay and
`JobConflict` over a v2 and over a `logical` job after two or more chunks;
`erasure_status` on a v2 job; the exact-scope barrier rows, including
receipts that predate the barrier, after chunks; barrier isolation of child
and sibling scopes; binding mismatch and a missing binding; the TTL race of
section 7 (clocks above the wall clock, the high-water poke modeled as
`high_water`) and the residue statement of section 5 (the evidence record of a
TTL-reclaimed memory stays detached; the content nodes of a reclaimed memory
of more than 4,096 bytes stay too, a core-level case that no served route
writes); and logical reopen. Maintenance cases
relabel the graph revisions (a checkpoint gives every node one value, a
compaction an arbitrary assignment) and assert errors and counts, never
revision numbers; they include a pre-barrier receipt, which fails closed with
`DeletionPending` after the relabel (section 6). Rust replays them on the
four storage backends and, for the maintenance cases, through a real
`Database::checkpoint()` followed by a reopen, and `maintenance::compact`, on
the stores that have them. A Python snapshot round trip is logical evidence,
not backend evidence.

Hand mutants the Rust slice must kill, each by a named test: ignore
`expected_revision`; ignore the generation; accept a token of another revision;
accept a token of another chunk count; replay returns the current job instead
of the stored outcome; the digest omits the token or the window; delete only
the memory node (dangling edges); skip content nodes; retain evidence by A2's
rule; skip the evidence delete; count guards wrongly (4,096 versus 4,095 or
4,097); count a shared edge twice; order by key instead of node ID; decode only
chunk members instead of the window; skip the content edge or node validation
or the content node's edge and attachment checks; mark `logical` while
`remaining` is unknown; allow a resume on a `logical` job;
check the deadline before the replay; probe the missing job before the
receipt; find the latest receipt by revision equality instead of `chunk_no`
(killed by a checkpoint-and-reopen case); and require E1's revision equality
for the begin receipt of a v2 job (killed by the ordinary v2 begin replay, where
the begin revision is below the job revision). Two mutants are equivalent
under `&mut Database` and are recorded, not killed (E1's ledger precedent):
drop the job revision guard in the frame, and reuse an expired receipt without
its replacement guard.

Coordinator-owned CI proposal next to the E1 commands:

```text
python tests/memory_erasure_resume_oracle.py --self-test
python -O tests/memory_erasure_resume_oracle.py --self-test
```

**Not frozen:** E3 entity target, entity barrier and edge-level chunks; E4
manifests, `manifest_id`, retention, reconciliation, lineage fencing,
`ReconciliationRequired`, receipt cleanup and the orphan sweep; the serving
mapping (HTTP 200 for `logical` and 202 for `pending`, error codes, request
and response fields); X1; every F-track item; multi-worker claims; secure
erasure and any backup, replica or feed claim. E2 is not whole C.

**E3/E4 scalability list (watch item, not E2).** The memory layer has no scope
index, so a chunk scans the `VliteMemory` nodes of every scope from the lowest
node ID (section 5): a small scope in a large tenant graph costs a scan of all
memory nodes per chunk and can reach the read ceilings (`OperationLimit`).
That is acceptable for E2; a scope index or an equivalent is an E3/E4 item.
