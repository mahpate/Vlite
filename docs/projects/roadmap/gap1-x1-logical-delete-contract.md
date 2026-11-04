# Gap 1 X1: consolidation logical-delete contract (proposed)

Status: **PROPOSED, not frozen or implemented.** This is an independent contract
candidate against `136d72c5a94e1c0d1878f99c461a555a91b7c3f6`. Every X1 choice
below requires coordinator ratification before Rust implementation. C1 v1 is
unchanged. No public API, wire, header, engine operation, dependency, activation,
default, erasure job or fact support is added.

Authority: [Gap 1](gap1-agent-memory.md) sections 4-6,
[frozen C1](gap1-c1-consolidation-contract.md) sections 1-8,
[C2](gap1-c2-remainder-contract.md) sections 5, 6 (X1/S1), 7 and 9,
and [memory serving](serving-memory-contract.md) sections 4-5.
The independent candidate oracle is `tests/memory_x1_oracle.py`, with additive
`tests/fixtures/agent_memory/x1_golden.json`. It models this proposal, not Rust
parity, storage durability or four real backends.

## 1. Boundary and requests

X1 creates one caller-produced Episodic summary and optionally logically deletes
some exact Entity/Episodic inputs, atomically with a versioned receipt. It never
infers deletion. C1 requests and receipts continue using their existing path.
Even an all-retain X1 request uses X1 v2, not a reinterpretation of C1.

Request scalars, normalization, error categories, collection binding, provenance,
content chunking, scope and clock rules are C1 section 2. Inputs are 1..256,
default budget 64, explicit budget at most 256. Each input must explicitly say
`retain` or `logical_delete`; duplicate IDs reject, then inputs sort by raw UTF-8.
The future serving adapter may materialize its retain default before this private
request is constructed. An input revision is the nonzero u64 **memory** revision,
not the graph revision; every changed existing graph object is also guarded by
its actual graph revision during planning.

One additional execution control is `dry_run: bool = false`. Dry-run reports
conflicts and physical/logical operation and encoded-body byte estimates, under
one captured view/C, but creates no summary, receipt, deletion, sequence, cleanup
or reservation. Structural errors still reject; state-dependent planning errors
become a singleton `conflicts` list, with unavailable estimates `null` rather
than invented zeroes. A conflict-free plan has `conflicts=[]`. No exhaustive
multi-conflict claim. Commit always replans; never pass a plan token as authority.
Dry-run strictly decodes an existing receipt under section 5 and never replays
it: a malformed receipt is the singleton conflict `InvalidBundle`; a live one
leaves current state to be planned; a valid expired one is charged exactly as
commit would replace it (replacement guards and obsolete suffix).

Serving reconciliation: `MemoryPlanResponse`
(`serving-memory-contract.md:690`) currently requires non-null
`user_operations` and `encoded_bytes`. This private result keeps unavailable
estimates `null`. Before X1 is served, the serving schema must either make both
nullable when `conflicts` is non-empty or define their conflict-time value. That
is a Gap 1 slice D serving decision needing the owner's wire approval, not an X1
byte.

## 2. Evidence, deletion and order

Resolve and validate every input at C before making any change. An occupied
summary identity or exact Episodic content/source collision still rejects,
including when that identity is also selected for deletion. Source expiry E<=C
is missing. Past summary expiry rejects before planning, as C1 does.

Apply A2 forget ownership rules to the complete set of delete inputs, not one
input at a time (`src/memory.rs:505-734,1605-1663` at the base):

- Inspect all target attachments. An undeclared attachment, different collection
  or different actual owner is `ForeignAttachment`; inconsistent metadata or a
  missing declared attachment is `InvalidBundle`. No implicit takeover.
- Detach each authorized target-owned record. Compute sharing from live retained
  memories in the exact scope, following live outgoing REFERENCES transitively,
  including paths through a delete target. A reachable target's evidence is
  shared too. Detach but retain/report every such record; delete only unshared
  target-owned records. Existing orphan records are not a cleanup opportunity.
- Enumerate both directions of every target's incident edges, deduplicating by
  edge ID, including self-loops. Delete all of them. Delete the target's owned
  HAS_CONTENT chunk nodes, then the target. An unexpectedly attached content
  chunk blocks deletion instead of detaching an external record.
- Do not delete retained memories or their content. Do not copy deleted source
  content, source URIs or source provenance into the summary's bookkeeping.
- Optional output evidence must already be attachable under C1 rules **before**
  deletion. Reusing an input's still-attached record as output evidence is
  `RecordAlreadyAttached`, not a transfer operation.

Canonical batch order: summary CreateNode; optional summary SetNodeExpiry and
its expiry guard; summary content nodes/HAS_CONTENT edges in chunk order; DERIVED_FROM edges for
retain inputs in canonical input order; optional output Attach; authorized
target Detach and unshared Vector Delete pairs by evidence-ID UTF-8 order;
incident DeleteEdge by increasing engine ID; owned content DeleteNode by key
UTF-8 order; target DeleteNode in canonical input order; obsolete expired
receipt outcome DeleteNode by index; new/replacement outcome nodes by index;
receipt node last. No edge is ever created to an input deleted in this frame.
DeleteNode/DeleteEdge and replacements use actual expected graph revisions.

Expiry guard: with summary expiry E, a second, byte-identical
SetNodeExpiry(summary, E) directly follows the first. From its deadline a node
is absent to every later operation of the same batch
(`src/property_graph/mod.rs:301-306`). If E is at or before the captured commit
clock, the guard finds the summary absent and the frame refuses before append,
even when no later entry references the summary (all-delete, no evidence, inline
content). It is a real 24-byte entry: one physical and one logical operation.

## 3. Exact v2 digest bytes (proposed freeze)

Use the C1 section 3 encoders U32, U64, I64, S, O, OU, B, F and SCOPE unchanged.
H is SHA-256; `hex` is lowercase without separators. X1 digest strings are
`"v2:" || hex(H(bytes))`. No C1 `v1:` digest is accepted as an X1 digest.

InputBytes = U32(N), followed by each canonical input's
S(id) || U64(expected_memory_revision) || D,
where D is one byte 0 for retain, 1 for logical_delete.

SourceBytes =
ASCII `"vlite.memory.consolidate-x1.sources.v2\0"`
|| SCOPE(ScopeBytes) || InputBytes.

RequestBytes =
ASCII `"vlite.memory.consolidate-x1.request.v2\0"`
|| SCOPE(ScopeBytes) || InputBytes
|| S(summary.id) || S(summary.content) || S(summary.source)
|| I64(observed_at_ms) || F(importance) || OU(expiry_ms)
|| U32(normalized_provenance.len) || each C1 binary provenance entry
|| S(transformation) || C1 EvidenceBytes
|| B(allow_future_observations) || U64(receipt_window_ms).

SourceDigest and RequestDigest hash their respective bytes. C, deadline,
input_budget, dry_run and idempotency_key are excluded. Changing a disposition
or receipt window changes the digest. RequestBytes over 16,777,216 reject before
receipt lookup. This is separate from the physical frame body limit.

The receipt operation is **`consolidate-x1`**. Preserve the existing
`operation_receipt_key` algorithm byte-for-byte:
`"receipt:v1:" || ScopeKey || ":" ||
hex(H(ASCII "consolidate-x1:v1\0" || UTF8(idempotency_key)))`.
The helper's key-envelope suffix remains v1; the operation/schema/digests are
new. Do not change the helper or use `"consolidate:v2\0"` with that helper.

## 4. Persisted provenance and stale status

Use the existing memory key, ordinary memory properties, content identity,
content chunks and labels from C1 section 4. Replace its four c1_* summary
properties with these properties:

| Property | Type | Value |
|---|---|---|
| x1_schema | String | `consolidate-x1:v2` |
| x1_transformation | String | normalized transformation |
| x1_source_digest | String | SourceDigest |
| x1_source_count | Integer | N |
| x1_source_chunks | Integer | number of manifest chunks, 1..6 |
| x1_sources_0000 through x1_sources_0005 | String | consecutive chunks of lowercase hex ManifestBytes |

ManifestBytes = U32(N) || entries sorted by raw ID-hash bytes.
Each entry is exactly H(UTF8(source.id)) (32 raw bytes)
|| U64(expected_memory_revision) || D (one byte).
Reject duplicate hashes as DigestCollision, even for different source IDs.
The scope is bound by the summary's ordinary scope property. N must be 1..256,
equal x1_source_count; revisions must be nonzero; D must be 0 or 1; entries are
strictly increasing by hash; exact length is 4+41*N bytes, with no trailing data.
The complete hex encoding is at most 21,000 bytes. Split it into consecutive
4,096-byte String properties, full except the last, indexed as four-digit
lowercase hex. Exactly x1_source_chunks properties exist; no missing or extra
indices, empty chunks, uppercase hex or trailing bytes. Concatenate before
decoding. This respects the **4,096-byte per-string ceiling**, not just the
65,536-byte total metadata ceiling. At most six properties are needed, and the
complete summary metadata and field count still must fit the engine ceilings.
These hashes are identity references, **not anonymization or secure erasure**.

Retain inputs get DERIVED_FROM edges using
`"derived-x1:v2:" || hex(H(ASCII
"vlite.memory.consolidate-x1.edge.v2\0" || S(summary_key) || S(source_key)))`.
Their String properties are exactly x1_schema=`consolidate-x1:v2` and
x1_source_memory_revision=canonical decimal expected memory revision.
No expiry or temporal segment is added to those edges.

An X1-aware status reader strictly decodes the manifest and edge properties.
For D=0 it requires exactly one matching retained edge and a live in-scope
source of the recorded revision; missing edge/source, expiry or changed memory
revision means stale. Extra, duplicate or wrong-target derivation edges are
InvalidBundle, not a guessed fresh status. For D=1 the persisted entry is the
receipt-frame proof of intentional deletion and contributes no retained-edge
requirement; an edge to that source is invalid. All-deleted inputs can therefore
produce a fresh summary. Status has no cascade or write side effect.

S1 is concurrently implementing **C1-only** status. It must not be changed here
or presumed to understand this schema. A C1-only strict reader must reject an
X1 summary as InvalidBundle until a separately authorized X1-aware reader lands.
Never write x1_schema into c1_schema or reduce c1_source_count to fool S1.

## 5. Receipt and outcome encoding

Receipt label VliteMemoryReceipt; exact properties:
String scope=ScopeKey, x1_scope_tuple=hex(ScopeBytes),
x1_schema=`consolidate-x1:v2`, x1_collection=bound collection or empty,
digest=RequestDigest, expires_at_ms=canonical nonzero decimal C+window;
Integer outcome_chunks=actual number of chunks.
No graph TTL on receipts or outcome nodes.

OutcomeBytes =
ASCII `"vlite.memory.consolidate-x1.outcome.v2\0"`
|| S(summary.id) || U64(1) || U32(N) || S(SourceDigest)
|| U32(deleted.len) || each deleted ID's S(id), in canonical input order
|| U32(retained_shared.len) || each retained record ID's S(id), UTF-8 sorted.
Both arrays have at most 256 entries and no duplicates. Deleted IDs cannot
include summary.id. These are replay identifiers, not copies of source content.

Encode OutcomeBytes as lowercase hex; split the ASCII into consecutive
4,096-byte chunks, full except the last, with no separators or newline.
There is at least one chunk. Keys use the existing receipt_outcome_key scheme,
indices 0..K-1. Label VliteMemoryReceiptOutcome, exact properties String
receipt_key, Integer index, String outcome. Changing an expired receipt to fewer
chunks deletes the obsolete suffix nodes in the same frame.

Strictly validate the whole receipt, all chunk labels/property names/types,
keys, contiguous indices, exact chunk sizing, canonical hex, and decoded
outcome bounds/ordering/UTF-8/version before deciding live versus expired.
Tuple mismatch is DigestCollision; collection mismatch is InvalidBundle.
After reading indices 0..K-1 (K=outcome_chunks), one absence probe of
receipt_outcome_key(key, K) detects an extra chunk (InvalidBundle). There is no
keyspace scan; a stray node at an index above K is neither detected nor
claimed to be. Live matching digest also
requires summary ID/count/source digest and deleted input IDs to match the
request; shared-evidence IDs are the original recorded result, never recomputed.
Live mismatch is IdempotencyConflict; live match replays before deadline, source
presence, observation and output TTL checks, without extending anything.
Expired malformed receipts still reject. Valid expired receipts permit fresh
planning; expired outcomes do not have to match the new request.
No cryptographic authentication against coordinated graph tampering is claimed.

Closure reads reuse the A2 forget paging under `GraphLimits::CEILING`
(`src/property_graph/engine/limits.rs:99-108`): live scope memories page through
find_nodes by 1,000 (`src/memory.rs:1281-1309`); REFERENCES out-neighbors page
by max_output_nodes, 1,000 (`src/memory.rs:1605-1657`); each target's incident
edges page in both directions by max_output_edges, 2,000, and refuse
OperationLimit once they exceed the operation cap (`src/memory.rs:552-584`).
An incomplete page without a cursor refuses OperationLimit: no partial closure
and no guessed sharing.

Private results. Created: status, outcome (id, revision, N, source_digest,
deleted, retained_shared), sequence, physical_ops, logical_ops, frame_bytes,
outcome_chunks, obsolete_outcomes, commit_frames=1. Replayed: status, the
recorded outcome, original sequence, receipt_revision, physical_ops=0,
logical_ops=0, commit_frames=0. Plan: status, conflicts, physical_ops,
logical_ops, frame_bytes (all three null with a conflict), commit_frames=0; a
conflict-free plan adds outcome_chunks and obsolete_outcomes. The oracle's entry
`roles` list is fixture evidence of canonical order, not a result field.

## 6. Bounds, guards, clocks and failure

C1 structural/error precedence applies, except explicit two-valued dispositions
and dry_run validation. After scope verification, commit and dry-run both
strictly decode and classify a receipt; commit replays a live matching one,
while dry-run never replays and plans current state (section 1). Fresh execution
checks deadline, observation, summary expiry, every input, output
identity/evidence, deletion ownership, then exact budgets.

User physical operations are the actual batch length, including the expiry
guard. Logical operations add: one guard per deleted target, plus one per
replaced receipt/outcome node.
No invented retain-input guards or separate charge for DeleteEdge revisions.
Both counts must fit 4,096. Count obsolete outcome deletes physically, without
an additional replacement charge. Reclaim entries are uncounted user operations
but byte-charged. Any over-cap plan rejects, never truncates or splits.

Body bytes = 4-byte header plus every encoded entry including reclaims.
Use C1 section 7 PutNode/PutEdge/Attach/SetNodeExpiry arithmetic.
DeleteNode=16, DeleteEdge=16, Detach=20, Vector Delete=28 bytes including their
8-byte envelopes (`src/property_graph/engine/codec.rs:264-294,511-566`).
The vector delete contains collection u32 plus a 16-byte inner batch delete.
Property metadata caps apply independently of total body cap.
No claim that the nominal 16 MiB frame boundary is attainable by every X1 shape.

Independent smallest-delete witness in the golden: scope (tenant `t`, agent `a`)
bound to collection `c`, key `k`, C=10, window=100, source `m` revision 1,
output `z`/`x`/`s` observed at 5, transformation `t`, no expiry/evidence.
RequestBytes=142, OutcomeBytes=140. Summary PutNode=912, source DeleteNode=16,
outcome PutNode=618, receipt PutNode=527 (x1_collection=`c`; empty would be
526); body=4+912+16+618+527=2,077 bytes. With summary expiry 12 at wall 11 the
same request adds SetNodeExpiry and its guard (24 bytes each):
body=4+912+24+24+16+618+527=2,125, physical 6, logical 7; expiry 11 refuses.
An opening reclaim adds 16 bytes, not a user operation.

Plan under the exclusive mutable borrow; commit captures
max(request C, wall, graph high-water) once and applies the batch in canonical
order at that clock. A node due at that clock is absent to every later entry:
the summary after its expiry operation (so the guard refuses), a delete or
retain input, an incident edge or either endpoint. Any such absence refuses the
whole frame before append, including for a delete input: never silently convert
a missing explicit target into success. Same-clock unrelated TTL reclamation can
accompany success, not rejection. No timer or callback between planning and
commit is introduced.

One frame contains summary, retained derivations, source deletions, evidence
decisions and outcome/receipt, or a preappend refusal changes none of them.
I/O CommitIndeterminate is not rollback: preserve it, stop using the poisoned
handle, reopen/recover and reconcile the same semantic request/key.
Reopen must preserve bytes, replay and status on plain/encrypted file and
plain/encrypted directory stores. Oracle JSON roundtrips do not qualify these
real engine backends; their integration tests remain required.

## 7. Ruling table (all choices PROPOSED)

| Question/options | Proposed choice | Rationale / authority |
|---|---|---|
| Widen C1 or separate v2 | Separate X1 operation/schema/digests, even all-retain | C2 section 5 rule 3 and decision 13; no C1 byte change |
| Missing disposition or explicit enum | Explicit private enum; serving fills retain | Preserve C1's explicit input discipline; serving section 4 |
| Drop deleted provenance, keep dangling edge, or manifest | Bounded hashed manifest on summary; only retain edges | No dangling edge and no false stale after intentional delete; C2 X1 |
| Raw IDs, hash manifest or one node per source | Fixed-width hash manifest sorted by hash, chunked over at most six String properties | Respects 4,096 bytes per String at 256 inputs without new nodes; memory keys already hash IDs; no privacy claim |
| S1 silently accepts X1 or separate decoder | C1-only S1 rejects X1 until separately extended | Do not alter concurrent S1 or C1 semantics |
| Mixed ownership or implicit evidence transfer | A2 ownership/REFERENCE closure; no output transfer | Gap 1 section 5 and A2 implementation |
| Plan replay versus current plan | Dry-run strictly decodes but never replays; a live receipt plans current state, an expired one is charged as its replacement; commit keeps replay precedence | Dry-run is current conflict/estimate evidence, not reservation |
| All conflicts or first conflict | Singleton first state conflict, unavailable counts null | Bounded deterministic evidence, never false complete plan |
| Digest dry_run/C/deadline/budget | Exclude execution controls; include dispositions/window | C1 request identity; plan does not consume idempotency key |
| Helper version versus digest version | Preserve helper envelope v1 with new operation name | Exact existing operation_receipt_key; v2 refers to X1 schema/digests |
| Fixed outcome or chunks | Canonical binary, hex chunks with strict suffix cleanup | Up to 256 deleted/shared IDs exceed C1's one-chunk bound |
| Existing-object guards | Actual revisions; charge delete-target and replacement guards; summary expiry guard is a repeated SetNodeExpiry (not PatchNode, which re-encodes the node; not a receipt-to-summary edge, which persists extra state; not a memory-layer check, which cannot see the commit clock) | A2 target guard budget, C1 replacement convention, `src/property_graph/mod.rs:301-306` |
| Separate deletion frame or one frame | One bounded frame or refusal | Gap 1 section 4, C2 section 5 |
| Barrier and erasure dependencies | Under an E1 barrier, `consolidate-x1` replays a live matching receipt committed before the barrier, otherwise refuses `DeletionPending`; dry-run refuses `DeletionPending`. S2 labels X1 summaries `stale: None` and never calls the C1-only S1. X1 and E1 merge serially (both edit `consolidate.rs`) | C2 E1 (`gap1-c2-remainder-contract.md:217-247`), S2 (`:186-203`), X1 (`:205-215`) |

## 8. Acceptance and not frozen

Independent goldens must exercise mixed/all-delete/all-retain, canonical input
order, revision conflicts, TTL boundaries, strict v2 encoding, receipt replay
after sources disappear, expired replacement, shared/foreign evidence and
transitive REFERENCES, full rollback, dry-run nonmutation/revalidation, exact
operation boundaries and v1 byte preservation. Mutation kills require a named
failing scenario and a restored pass. Surviving/equivalent changes are reported,
not counted as kills. The future Rust tests must include an all-delete request
without evidence whose summary expiry equals the commit clock: the expiry guard
refuses it before append and state is unchanged.

Not frozen: public names, transport shapes, S1/E1 hooks, facts, scope erasure,
deletion manifests for backup consumers, authorization adapters, backend
qualification, performance and release eligibility. This proposal is not an
approval to expose logical_delete or to claim whole slice C complete.
