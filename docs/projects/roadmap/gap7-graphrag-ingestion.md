# Gap 7: GraphRAG ingestion

Status: design for owner approval; nothing in this document is implemented.
This document is conformed to the [serving wire conventions proposal](serving-wire-conventions.md), whose owner decisions remain open.
Scope: one source document becomes embedded chunks, resolved entities, directed
relations and evidence in a graph-enabled database. All proposed defaults, Rust
types, reserved schema and delivery boundaries require approval before code.
Current facts below were verified in this checkout; older design citations are
not substitutes for current code. No serving wire is approved by this document.

## 1. Scope and requirements

Build on [the next-major roadmap](next-major.md), [Gap 1 memory](gap1-agent-memory.md)
and [the Gap 8 serving contract](serving-contract.md). Non-goals: crawling, URI
fetching, PDF/OCR parsing, semantic entity merging, model training, community
summaries, a scheduler, cross-file transactions, automatic provider discovery and
new storage opcodes. Input is already-decoded UTF-8, not arbitrary file formats.

1. **ING-001:** identical source bytes and pinned pipeline produce identical chunk
   spans, normalized identities and operation order on Windows, Linux and macOS.
2. **ING-002:** the default mutation commits the entire document, evidence and
   receipt in one op-12 frame, or writes nothing; oversize is an explicit error.
3. **ING-003:** no extraction or embedding call leaves the process unless the
   operator explicitly installs and authorizes a Gap 8 provider binding. Gap 8 as
   written defines embedding and reranker providers only (C15), so external
   extraction stays out of scope until Gap 8 adds an extraction-provider kind.
4. **ING-004:** exact receipt replay performs no provider calls, writes, merges or
   retention extensions. A conflicting prepared request never overwrites a receipt.
5. **ING-005:** every extracted entity/relation has validated source byte evidence;
   every vector maps to exactly one chunk node and source revision.
6. **ING-006:** tenant, graph, collection and provider authorization precede work;
   extraction cannot choose identities outside that scope or change authorization.
7. **ING-007:** reject invalid, oversized, timed-out or conflicting output without
   silently truncating, skipping chunks, substituting vectors or switching models.
8. **ING-008:** explicit incremental mode commits bounded frames with receipts;
   interruption exposes recorded partial progress, never document-wide atomicity.
9. **ING-009:** replacement/deletion removes only the addressed source's evidence;
   shared entity anchors and unrelated sources survive, including after replay.
10. **ING-010:** attached records never expire; retention uses explicit guarded
    graph/vector cleanup, with restartable receipts for multi-frame deletion.
11. **ING-011:** independent reference outputs, mutation tests and crash/reopen
    tests establish the contract before public adapters are enabled.
12. **ING-012:** a `Partial` job can be aborted by a guarded, receipted, bounded
    cleanup that needs no caller bundle, replays idempotently, is authorized like
    delete, makes no provider call and never restores already deleted old content.

## 2. Verified current coverage

Evidence labels apply only to the facts in this table. Everything after it is
proposed unless explicitly attributed to one of these rows.

| Key | Current coverage | Verified evidence |
|---|---|---|
| C1 | Retrieval validates bounds, hybrid-searches a pivot collection, revalidates records and maps attachments to live graph nodes | `src/property_graph/retrieve.rs:301-438` |
| C2 | `Embedder` accepts bytes/text; registry names resolve providers; local `HashEmbedder` is deterministic hashing, not a semantic model | `src/embedding.rs:1-55` |
| C3 | Optional HTTP embedder has a 30-second timeout, no redirects and a bounded response; this is not the proposed managed registry policy | `src/embedding.rs:66-96`; `src/embedding.rs:110-130` |
| C4 | Bulk import commits accepted rows with a SHA-256 checkpoint; repeated last chunk/digest is recognized and noncontiguous chunks conflict | `src/import.rs:1-6`; `src/import.rs:676-722` |
| C5 | Plain vector batches target one collection and append one batch frame for that collection | `src/lib.rs:2103-2133` |
| C6 | Graph commits validate/encode, append op 12, then apply graph state and vector mutations across their collections | `src/property_graph/mod.rs:724-747`; `docs/STORAGE-FEATURES.md:193-202` |
| C7 | Graph batch limit is 4,096 user operations and 16,777,216 encoded body bytes; internal TTL reclaims are planned at commit from the clock, do not count against the 4,096, add entries and consume body bytes | `src/property_graph/mod.rs:397-402`; `src/property_graph/mod.rs:601-610`; `src/property_graph/engine/limits.rs:31-35`; `src/property_graph/engine/mutate.rs:297-319`; `docs/STORAGE-FEATURES.md:184-188` |
| C8 | Graph operations include detach, vector mutation and guarded node writes; returned revisions identify committed outcomes | `src/property_graph/mod.rs:300-309`; `src/property_graph/mod.rs:331-394`; `src/property_graph/mod.rs:430-455` |
| C9 | Graph keys have 256-byte caps; properties are scalar, at most 64 fields / 65,536 encoded bytes, strings at most 4,096 bytes | `src/property_graph/engine/limits.rs:14-30`; `src/property_graph/engine/model.rs:22-33` |
| C10 | Attached records cannot be deleted/expired before detachment; graph nodes and edges now can have expiry | `docs/STORAGE-FEATURES.md:196-207`; `src/lib.rs:442-450` |
| C11 | A write/sync failure may be indeterminate and poison the handle; periodic durability is not full power-loss durability | `src/lib.rs:2103-2108`; `docs/STORAGE-FEATURES.md:242-251` |
| C12 | Collection wildcard does not grant graph scope; mixed graph/vector writes need graph, write and collection grants; tenant graph names are qualified; structural graph reads return no vector record data, only `retrieve` does, under per-collection evidence grants | `src/server/graph.rs:12-56` |
| C13 | Existing errors cover bounds, revisions, invalid queries, indeterminate commits and graph integrity | `src/lib.rs:303-366`; `src/lib.rs:415-471` |
| C14 | Gap 1 specifies keyed graph receipts in the same mutation frame, with reopen reconciliation, retention only by explicit policy, and no promise of indefinite replay; its core source is absent here | `docs/projects/roadmap/gap1-agent-memory.md:111-126` (`src/memory.rs` absent) |
| C15 | Gap 8 specifies operator bindings, egress controls, bounded adapters and no failure fallback for embedding and reranker providers only, with no extraction kind; registry implementation is not claimed | `docs/projects/roadmap/serving-contract.md:60-127` |
| C16 | A graph-batch vector put is an unconditional upsert of a caller-chosen u64 record ID; `Mutation` carries no `Expected` precondition and the engine has no ID allocator; an in-crate lookup can check absence, treating an expired record as absent | `src/property_graph/mod.rs:847-860`; `src/lib.rs:779-793`; `src/lib.rs:1853-1866` |
| C17 | Compaction, upgrade and restore assign a new lineage; compaction restarts sequences; a node's revision is the sequence of the last frame that wrote it, so a patch re-stamps it | `src/lib.rs:1575-1577`; `docs/STORAGE-FEATURES.md:221-223`; `src/property_graph/engine/state.rs:1032`; `src/property_graph/engine/state.rs:1078` |
| C18 | `GraphKeyConflict` is the edge-key conflict; `UniqueTriple` allows one edge per source/type/destination and `ensure_edge` requires it; the default multiplicity is `Multi` | `src/lib.rs:429-431`; `src/lib.rs:683-686`; `src/property_graph/engine/schema.rs:191-195`; `src/property_graph/engine/schema.rs:944-954`; `src/property_graph/engine/mutate.rs:504-514` |

Consequently, graph-enabled files can atomically commit vector writes across
collections plus graph changes in **one op-12 frame** (C6). Plain batches commit
one frame per collection (C5), not a database-wide transaction. Reuse the Gap 1
receipt pattern (C14), not a separate KV write; borrow import's checkpoint idea
(C4), not its collection-specific marker as a mixed graph transaction.

## 3. Exact model and canonical identities

Provision one ingestion graph and one chunk collection per authorized binding,
outside ingestion atomicity. A binding is the stored graph identity, stored
collection identity and tenant, with an immutable generation. Database lineage is
checked at authorization and carried in resume tokens, never hashed into keys or
identities: compaction, upgrade and restore assign a new lineage and renumber
revisions (C17). Reprovisioning a dropped/recreated graph or collection requires a
new generation; never resume by a reused name. No graph or collection is allocated
per document.

Proposed embedded input/output, not a public wire:

```text
Source = { tenant: utf8[1..128], source_id: utf8[1..256],
           text: utf8[1..1048576], uri?: utf8[0..2048] }
Pipeline = { version: sha256, chunker, extractor_alias, extractor_version,
             normalization: "ascii-key-v1", embedder_alias, model_version,
             dimensions, binding_generation, schema_version: 1 }
IngestRequest = { source, pipeline, expected_source: Absent | Revision(u64),
                  mode: AtomicDocument | Incremental, deadline_ms: uint64 }
Prepared = { receipt_key, request_digest, plan_digest, ordered_batches }
IngestResult = { receipt_key, source_revision_key, state, last_batch, total_batches,
                 chunk_count, entity_count, relation_count, receipt_revision,
                 original_outcome: Created | Replaced | Deleted | Aborted }
state = Partial | Complete | Superseded | Deleted | Aborted
```

UTF-8 length bounds are bytes. IDs/URIs reject NUL; URIs are inert and never
dereferenced. Pipeline version is the SHA-256 of the entire canonical pipeline
configuration, including strategy, overlap, deterministic extractor rules,
provider/model versions and semantic bounds; execution deadline is excluded.
Use original source bytes: no newline, BOM or Unicode normalization. Content
digest is SHA-256 of these bytes. A binding generation change requires a new
pipeline version; compaction, upgrade and restore do not and instead fail closed
(section 5).

Canonical encoding is version byte 1 followed by schema-ordered fields; strings
and byte arrays have u32 little-endian lengths, integers are fixed-width
little-endian, optional values have a 0/1 presence byte. Lists have u32 counts;
maps sort UTF-8 keys by unsigned bytes. Typed floats encode finite IEEE bits,
normalize negative zero to positive zero and never depend on locale/JSON order.
All hash inputs have a length-framed domain separator. Digest text is lowercase
64-character hex; graph keys are `g7_<kind>_<digest>`, below C9's key cap.
Persist and compare the bounded canonical identity fields on lookup; a digest
collision is a conflict, never an identity match.

| Reserved object | Identity and stored fields |
|---|---|
| `G7Source` head | `(binding, tenant, source_id)`; the only mutable ingestion object: current revision key, revision digest, pipeline, status, current job receipt key and last committed batch ordinal |
| `G7Revision` | `(source head, content_digest, pipeline.version)`; immutable source identity, URI, text length, chunk count and receipt key |
| `G7Chunk` | `(revision, ordinal, start_byte, end_byte)`; source/revision keys, ordinal, half-open byte span, chunk digest, pipeline version |
| Chunk vector | Collection u64 record ID chosen by the planner under the allocation method of section 10, stored in the frozen plan and required absent under each creating frame's exclusive borrow (C16); payload is exact chunk bytes, metadata contains tenant/source/revision/chunk keys, and expiry is absent |
| `G7Entity` | `(binding, tenant, kind, normalized_key)`; only immutable kind and normalized key, no mutable display name or inferred truth |
| `HAS_REVISION`, `HAS_CHUNK` | Directed head-to-revision and revision-to-chunk links, keyed by their full endpoint tuples |
| `MENTIONS` | Chunk-to-entity edge keyed by `(chunk, entity, evidence_span)`; stores original surface, confidence if supplied and extractor version |
| Extracted relation | Entity-to-entity edge keyed by `(revision, chunk, subject, predicate, object, evidence_span, canonical_attributes)`; properties include chunk/revision/source keys, evidence span and extractor version |
| `G7Receipt` | `(binding, tenant, source_id, content_digest, pipeline.version)`; created once in the first frame and immutable: request/plan digests, batch count, source revision key, outcome and planned aggregate counts; `state` is never stored |
| `G7BatchReceipt` | `(root receipt, batch ordinal)`; immutable: planned batch digest, `final` flag, committed result-ID mapping and created versus reused anchor counts; its graph revision is the freshness token `receipt_revision`, never a predicted next sequence |

All relation kinds are provisioned schema entries, not arbitrary executable query
text. Reserved identity fields cannot be overridden by extractor attributes.
Properties use C9's scalar representation; no array-valued properties are assumed.
Leave `MENTIONS`, `HAS_*` and predicate relationship types at the default `Multi`
multiplicity and never use `EnsureEdge`: `UniqueTriple` would reject a second edge of
one type between the same endpoints, which distinct evidence spans require (C18).
No unbounded provenance arrays.

Result-ID mappings and ownership entries are separate keyed shard nodes created in
the frame that creates the objects they describe. Each shard is one node with one
string property of at most 4,096 UTF-8 bytes (C9) and receipt-key and shard-index
back-references that replay validates; each counts as an ordinary operation.
Receipts and shards are never patched or expired. This follows the receipt-outcome
chunk pattern of Gap 1's A1 revision (Gap 1 section 2, pending acceptance and absent
from this checkout's older copy); Gap 7 depends on that pattern only, never on Gap 1
internals. Every created/removed record, edge and node has a deterministic ownership
entry in the prepared manifest; persist live entries in shards that never span
dependency groups, so replacement, deletion and abort reconstruct bounded cleanup
input without old inference or a caller's old bundle. Shared anchors are marked
retained, not source-owned.

Only the head changes after creation, and a patch re-stamps a node's revision (C17),
so `state` is derived at replay, never stored on a receipt. `Partial`: the head names
this job and its final batch receipt is absent; an abort in progress leaves it
`Partial`. `Deleted` or `Aborted`, checked before `Superseded`: the matching delete or
abort receipt's final batch receipt exists. `Complete`: the final batch receipt exists
and the head's current revision is this receipt's. `Superseded`: otherwise, the head's
current revision is a later one. `receipt_revision` is the last committed batch
receipt's current graph revision, Gap 1's "commit sequence": a freshness token stable
only within one lineage, never an identity.

## 4. Pipeline: inputs, outputs, bounds and failures

### Admission and deterministic chunking

Authorize the binding and provider aliases before reading source text into a job.
Validate UTF-8 and all bounds before provider work. Require a pinned pipeline and
explicit embedder; do not silently select hashing for a semantic-model request.
Capture current source revision and receipt state under a short guard, then
release it before extraction/embedding.

Chunker inputs are original bytes, strategy, maximum chunk bytes `B` and overlap
bytes `O`. Require `1024 <= B <= 16384`, `0 <= O <= min(1024, B/4)`; defaults are
`B=4096`, `O=256`, strategy `ParagraphV1`. Output is ordered
`{ordinal:u32, start:u32, end:u32, text:&str, sha256}`, at most 512 chunks.
Reject excess, never keep a prefix. Spans are half-open offsets into original
bytes; all cut points must be UTF-8 scalar boundaries.

`FixedUtf8V1`: from start `s`, choose the largest scalar boundary `e` at or below
`min(s+B, len)`. `ParagraphV1`: start with that hard end; if not at EOF, choose
the largest boundary in `[s+ceil(B/2), e]` immediately after literal `\n\n`;
otherwise keep the hard end. CRLF has no special treatment. Append `[s,e)`.
At EOF stop; otherwise next start is the smallest scalar boundary at or above
`e-O`, strictly greater than `s`. This rounds overlap down, never exceeds O,
preserves full byte coverage and guarantees progress. No sentence/token/model
segmentation in v1. Whitespace-only text is valid; empty text is invalid.

### Extraction

Proposed extension point, using existing `Result`/`Error` only:

```rust
trait Extractor: Send + Sync {
    fn extract(&self, chunk: ChunkInput<'_>, context: &ExtractContext)
        -> crate::Result<Extraction>;
}
```

`ChunkInput` contains immutable chunk/revision IDs, ordinal, original source
start/end and exact text. `ExtractContext` contains pinned rule/model version,
allowed entity kinds/predicates, monotonic deadline and cooperative cancellation;
it contains no database handle, authorization token or arbitrary URL.
`Extraction` has entity observations
`{local_id:u32, kind, key, surface, span:[u32;2], attributes, confidence?:f64}`
and relation observations
`{subject_local_id, predicate, object_local_id, span, attributes, confidence?}`.
Spans are chunk-relative nonempty UTF-8-aligned ranges; surface must equal the
entity span's bytes. Convert to source offsets by checked addition.
Relations must reference emitted entity observations, not arbitrary stored IDs.
Confidence is optional finite `[0,1]`, never a truth/authorization signal.

Built-ins are local and deterministic: `LiteralDictionaryV1` matches configured
UTF-8 literals at every scalar boundary, including overlaps, in byte-offset then
rule-ID order; each rule supplies kind/key. `DictionaryRelationsV1` adds configured
directed predicate rules between dictionary observations in the same literal-LF
line, subject/object span union at most 1,024 bytes. Enumerate ordered endpoint
pairs, exclude a pair only when it is the same observation, then sort/deduplicate
exact observations. No probabilistic named-entity claims or regex dependency.
Zero matches produces vector chunks with no entities, not fabricated entities.

External extractors are out of scope until Gap 8 adds an extraction-provider kind
(C15 has none; section 9 blocks `gap7-providers` on that amendment). If it is
accepted, they use only that registry alias with a pinned version and tenant/binding
grant; until then only local in-process extractors exist. No ambient credential
discovery, implicit network adapter or caller-supplied endpoint. Decode only the above
schema, reject unknown fields, invalid references, spans, kinds or attributes.
Per chunk: at most 256 entity observations, 512 relations and 1 MiB decoded output;
attributes at most 16 scalar fields / 8 KiB encoded per observation, each string
at most 4,096 bytes. Overflow rejects the entire preparation, not a partial graph.
The same bounds apply to local extractors. Timeout/cancellation/busy are explicit
failures; accepted output is frozen before any mutation.

Boundary: Gap 4 gives memory extraction, fact-slot naming and contradiction policy to
the memory layer, and Gap 1 section 2 has the caller supply extraction
(`docs/projects/roadmap/gap4-temporal-graph.md:224`). Gap 7 extractors feed only the
ingestion graph with source-local evidence and never set Gap 1 slot, contradiction or
retention policy.

### Resolution, evidence and embedding

Resolution input is the validated observations. Kind/predicate names are exact,
case-sensitive provisioned identifiers. `ascii-key-v1` trims leading/trailing
ASCII space, tab, CR and LF, collapses interior runs of those bytes to one space,
and lowercases ASCII A-Z only. Preserve all other UTF-8 bytes; no locale-dependent
casefold, NFC, transliteration, synonym or fuzzy/vector merge. Reject an empty or
over-256-byte normalized entity key; preserve original surface on `MENTIONS`.

Equal `(binding, tenant, kind, normalized_key)` resolves to one immutable anchor.
Repeated exact mentions/relations deduplicate by their full canonical identities.
Different attribute maps are distinct relation assertions, not a last-writer-wins
fact. Entity descriptive attributes stay on mention evidence, never overwrite the
anchor. Duplicate same-identity observations with differing confidence or evidence
properties conflict; require the operator to change rules or explicitly curate.
An existing anchor whose full stored identity disagrees fails
`InvalidQuery("ingestion identity conflict")` (`GraphKeyConflict` is the edge-key
error, C18).
No cross-tenant resolution or automatic contradiction adjudication.

Output includes chunk-to-entity mention edges, direct relation edges and their
source/chunk keys. Every relation's evidence must belong to its own chunk, and
both endpoints must have mentions there. Following relation.chunk_key,
chunk.revision_key and revision.source identity yields the exact original span.
These property references are validated by the ingestion planner; do not claim
the engine has foreign keys from properties to edges. Provenance is source-local,
so removing one source cannot remove another source's relation evidence.

Embed each exact chunk text once through its pinned configured `Embedder` (C2).
Input is at most 16,384 UTF-8 bytes; output is one finite vector of the bound
collection's exact dimension. Invalid dimension/nonfinite output aborts; no zero
vector, skipped chunk or fallback model. Attach the non-expiring vector record
to its chunk node only, not to every mentioned entity (C8/C10).

### Freeze, plan and commit

Preparation produces a canonical plan including accepted extractor output and
vector bits, bounded to 64 MiB total scratch/plan memory; fail before exceeding
the bound. An external model need not be deterministic: determinism applies to
chunking, resolution and ordering for a fixed accepted output. The caller retains
the prepared bundle for incremental resume; no automatic plaintext disk spool.
Return its digest and require the exact bundle on resume. Provider/model version
alone is not evidence that regenerated output is identical.

Use two explicit embedded steps: `prepare` returns the complete frozen bundle
without appending; the caller retains it before invoking `commit(prepared)`.
Preparation's short exclusive planning guard chooses candidate record IDs under the
allocation method of section 10; they are not reservations, because the engine has no
allocator and a graph-batch vector put is an unconditional upsert (C16). Under every
frame's exclusive borrow, before its append, require each record ID that frame creates
to be absent; otherwise append nothing, fail `InvalidQuery("ingestion record id
conflict")` and keep any recorded partial job. `AtomicDocument` is one frame under one
borrow, so its check and append are indivisible.
Under exclusive database access, reauthorize, compare binding generation, the lineage
recorded in the bundle and the expected source revision, and check all affected
revisions. Freeze IDs, cleanup ownership and batch boundaries before first append.
Canonical ordering: guarded source/job state; old evidence edge removals; detach
and delete old vector records; old chunk/revision removals; new revision and entity
nodes sorted by key; chunks by ordinal with vector put before attachment; provenance
edges; relation edges sorted by canonical identity; receipt and ownership shards; the
batch receipt; head progress/state; and, in the first frame only, the root receipt
last. Retained existing anchors are not rewritten.
Every changed existing graph object uses its captured revision; new receipt/head
creation uses `Expected::Absent`. Each frame's final checks and append share the
exclusive borrow; a prepared bundle is not a lock across calls or frames.
For later frames, the head guard is a symbolic reference to the previous batch
receipt's revision (that frame created the receipt and last wrote the head, so the two
revisions are equal); resolve it to a u64 under that frame's guard. Never predict
sequences, and exclude those runtime substitutions from the canonical plan digest.
Their fixed-width encoding still permits exact preflight.

The one-active-mutation-per-source rule (section 6) orders only ingestion jobs, not
other sources or raw writers, so re-read shared anchors under each frame's borrow:
absent creates it; present with the identical stored identity omits the create and
references it; a different identity fails `InvalidQuery("ingestion identity
conflict")`. Preflight charges every referenced anchor as a possible create and the
plan carries each anchor's full identity. Omitting a create changes only the executed
operation list, never the plan digest; the batch receipt records created versus reused
anchors and carries the planned batch digest. If a raw writer changes guarded state
between frames (`RevisionMismatch`, `GraphKeyExists`, `GraphNodeNotFound`,
`GraphEdgeNotFound`, `GraphNodeHasEdges`; C13), the frame appends nothing and the
recorded partial job remains; abort (ING-012) is the exit.

Run exact engine validation/encoding of every frozen frame and include receipts,
guards, vector bytes and graph properties in frame admission (C6/C7). Reclaims are
planned at commit from the clock, so preflight cannot see them (C7): ingestion plans
no expiry entries and reserves the bytes of one opening reclaim per frame. A frame that
still fails `BatchTooLarge` at commit appends nothing and leaves any recorded partial
job. Validation failure appends nothing. Append via the existing mixed graph batch,
not parallel collection writes. Atomic visibility is one frame; durable
acknowledgement additionally requires Full durability or a successful explicit
sync (C11). Never promise an unconditional power-loss guarantee.

## 5. Receipts, replacement, deletion and retention

Receipt lookup is scoped by binding plus the required tuple
`(tenant, source_id, content_digest, pipeline.version)`. The request digest also
covers URI, mode, expected source revision and policy. A matching complete receipt
returns the stored result IDs/counts and the final batch receipt's current
`receipt_revision` without providers or writes. A different request/plan under the
same key fails `InvalidQuery("ingestion idempotency conflict")`; deadlines may
differ. Look up receipts before provider dispatch. A partial receipt requires
the original frozen plan, not fresh inference.

Each frame commits its effects, batch receipt and head progress in the same op-12
frame; the root receipt is created in the first frame only. Batch ordinals start at 1
and must be contiguous. Matching committed batch
digest is a no-op; mismatching digest, skipped ordinal or changed plan is a
conflict. After `CommitIndeterminate`/`Poisoned`, stop using the handle, reopen and
inspect the head and batch receipts: presence reconciles the complete frame; absence
permits retry on a healthy handle. Do not retry an uncertain append blindly. Errors
before any frame leave no job. After progress, errors preserve a resumable partial
job. Compaction, upgrade and restore renumber revisions (C17), so every frozen plan
and resume token carries the lineage it was prepared under; a mismatch fails closed
with `InvalidQuery("ingestion lineage changed")` and a partial job can then only be
aborted (ING-012). No receipted re-key is offered (section 10).

Re-ingestion with changed bytes or pipeline creates a new revision and receipt,
guarded by the current source-head revision. Default replacement deletes old
chunks, their vectors, mention/provenance edges and source-owned relation edges;
it retains source identity, immutable shared entity anchors and compact receipts.
It does not retain old text/vector history. New version creation plus old cleanup
must fit the same atomic frame in default mode; otherwise reject before append.
A superseded receipt replays as `Superseded`, derived from the head and never by
patching the old receipt, with its original outcome and a flag
that old object IDs may no longer be live; it never reactivates deleted content.

Delete uses a versioned action digest and separate receipt domain
`(binding, tenant, source_id, "delete", expected_source_revision)`, with the same
root/batch receipt shapes as ingestion. Remove owned
edges before chunks/revision, detach before vector delete, and tombstone the
source head in the final frame while creating the delete receipt; the ingestion
receipt stays immutable. Exact retry is a no-op. Delete is refused while the head is
`Partial`; abort first (ING-012).
Replaying a deleted ingestion returns `Deleted`, not a resurrection. Deliberate
restoration of identical bytes requires a new pipeline version. Shared entity
anchors remain; garbage collection needs a separate explicit bounded operation
checking all incident edges, never an unchecked reference-count decrement.
Deletion is logical live-state erasure, not a claim that old log/backup bytes
have been securely erased; physical retention follows an operator's maintenance
and backup policy.

Keep receipts/tombstones until explicit operator purge; purge forfeits the replay
promise and requires all source content/jobs already removed. No implicit eviction
or TTL. This matches the key integrity constraint in H-4, REQ-014 and LEG-004:
**records attached to graph nodes never expire**
(`docs/projects/feature-agent-graph/feature-agent-graph.prd.md:269`;
`docs/projects/feature-agent-graph/feature-agent-graph.prd.md:303`;
`docs/projects/feature-agent-graph/feature-agent-graph.prd.md:372`).
The older REQ-014 also prohibited graph TTL; current graph-node/edge TTL supersedes
that part (C10), not the attached-record rule. Set no graph TTL in ingestion v1:
expiring a node is not evidence-vector cleanup. Reject requested source/chunk TTL
and use explicit deletion jobs. No automatic retention worker is implied.

Receipt retention diverges from Gap 1, which allows receipt expiry only under an
explicit retention policy and advertised retry window and promises no indefinite
replay (C14). Gap 7 keeps receipts, result shards and tombstones until explicit purge
and claims no growth bound; purge mechanics, size accounting and any retention window
are a section 10 owner decision, and no purge operation is proposed for code until it
is decided.

## 6. Bounds, splitting and error taxonomy

These are proposed admission ceilings, not performance or capacity claims.
Operators may lower them, never silently clamp caller input.

| Resource | Ceiling / behaviour |
|---|---|
| Source | 1,048,576 original bytes; 512 chunks; no empty source |
| Extracted document | 4,096 distinct entities, 8,192 distinct relation assertions, 16,384 mention edges; reject overflow |
| One atomic frame | 4,096 total user operations, at most 16,777,216 encoded body bytes; every entity/edge creation costs an operation, so neither has an independent 4,096 allowance |
| Frame admission | Entity/edge counts must also fit after vectors, attachments, source, receipts and cleanup; one chunk's dependency group must fit alone |
| Incremental document | At most 64 frames and 131,072 total user operations including cleanup/receipt shards; preflight all frames |
| Job admission | At most 1 active mutation (ingestion, resume, delete or abort) per source, 2 preparations per tenant, 8 per process; no unbounded wait queue. The slot orders only ingestion jobs, not other sources or raw writers, hence the per-frame re-checks in section 4 |
| Extraction/embedding | At most 2 in-flight calls per job and 8 per provider, also charged to tenant/process admission; completion order never sets commit order |
| Provider call | 1 MiB encoded request, 4 MiB raw response, extraction additionally 1 MiB decoded; 2 s connect / 10 s total including queue and retry |
| Job time | Caller deadline at most 120 s per invocation, includes preparation/commit; resumed invocation gets a new bounded deadline |
| Cancellation | Check before each stage/call/frame and every 64 local observations; cannot cancel an already appended frame |
| Retry | At most one replay-safe connection/429/503 retry within the same call deadline; no auth/shape/timeout retry, no fallback |
| Prepared memory | 64 MiB including text, vectors, extraction, manifest and encoder scratch; checked arithmetic before allocation |

Local extractors must cooperate with cancellation; a Rust trait cannot forcibly
stop a malicious blocking implementation. Built-ins check deadlines; custom
in-process implementations are trusted operator code, never loaded from source
text. Registry adapters must enforce transport deadlines and bounded decoding.

Default `AtomicDocument` refuses an oversized document with `BatchTooLarge`;
it never silently splits. Explicit `Incremental` uses the frozen canonical plan:
greedily pack whole dependency groups (one chunk's vector, attachment, new anchors
and evidence) while reserving exact receipt/progress bytes. Cleanup groups are one
ownership shard with the owned edges it lists, or one chunk plus detach/vector delete
after its edges are gone.
New shared anchors precede their first reference; reuse them in later frames.
Split receipt mappings if needed. If any indivisible group or total ceiling fails,
reject before the first append. Existing-state drift at a later frame fails that
frame's guards (section 4), appends nothing and leaves the recorded partial job, not a
fresh automatic replan.

Each successful frame is immediately visible, including to ordinary graph reads
and raw vector search. **No staging invisibility or atomic activation is claimed.**
The first frame marks the head `Partial`, blocks a competing source mutation and
records the frozen plan digest; the final frame marks `Complete` and updates the
current revision. Replacement/deletion can therefore expose partially removed
old data and partially added new data. Every object carries its revision/source
keys; ingestion-aware readers must check head/job status and report incomplete
sources. Existing retrieval C1 does not perform that new join automatically.
Applications requiring all-or-nothing reads must use `AtomicDocument`; filtering
only the head is not a retrofit transaction. Require explicit acceptance of this
consistency model.

**Abort (ING-012).** A `Partial` job can be aborted by a guarded, receipted cleanup
within the Incremental frame limits. It runs from the ownership shards alone, so it
needs no caller bundle and no provider call, and it is authorized like delete
(graph-write plus every touched collection's write grant). It is the source's one
active mutation: each abort frame patches the head, so a concurrent resume fails
`RevisionMismatch`, and delete is blocked while a job is `Partial`. Each frame deletes
a bounded, canonically ordered set of objects the job created (edges first, then
detach before vector delete, then chunks and revision) together with the shards that
list them, never a retained shared anchor or another source's object, and creates an
immutable abort batch receipt; the final frame sets the head `Aborted`. The receipt
domain is `(binding, tenant, source_id, "abort", job receipt key)` and replay returns
`Aborted`. Abort never restores already deleted old content; what remains of the old
revision stays owned and can be replaced or deleted. Re-ingesting identical bytes
after abort or delete needs a new pipeline version because the original receipt key
is reused.

| Failure | Existing public `Error`; outcome |
|---|---|
| Invalid text, pipeline, span, reference or deadline | `InvalidQuery(&'static str)` with fixed sanitized reasons; no writes for preparation failures |
| Source bytes over 1,048,576 | `PayloadTooLarge(actual)` (Display at `src/lib.rs:498-500` states that fixed maximum, so an operator-lowered source ceiling uses the document-ceiling reason below) |
| Whole-plan ceilings (chunks, entities, relations, mentions, frames, prepared memory) | `InvalidQuery("ingestion document ceiling")`; no writes |
| A real frame limit (`AtomicDocument` over 4,096 operations or 16 MiB, or commit-time frame admission) | `BatchTooLarge` (Display at `src/lib.rs:501-504`); appends nothing, any partial job is kept |
| Invalid embedding or vector metadata | `InvalidDimensions`, `DimensionMismatch`, `NonFiniteVector`, `InvalidMetadata(&'static str)`; preparation failure, no writes |
| Receipt key reused with a different request/plan digest | `InvalidQuery("ingestion idempotency conflict")` |
| Anchor/head/revision/shard identity disagreement; resume after a lineage change; record-ID collision | `InvalidQuery("ingestion identity conflict")`, `InvalidQuery("ingestion lineage changed")`, `InvalidQuery("ingestion record id conflict")`; fixed reasons, no source text |
| Expected source/object revision changed | `RevisionMismatch { current }`; preserve committed partial progress |
| Raw-writer interference at a later frame (changed or removed object, anchor create race) | `RevisionMismatch`, `GraphKeyExists`, `GraphNodeNotFound`, `GraphEdgeNotFound`, `GraphNodeHasEdges`; the frame appends nothing, the partial job is kept and abort (ING-012) is the exit |
| Missing graph/collection or graph enablement | `GraphNotFound`, `CollectionNotFound`, `GraphsNotEnabled` |
| Graph schema/attachment/expiry conflict | Existing `GraphSchemaViolation`, `GraphAttachmentConflict`, `GraphRecordNotAttachable`, `GraphAttachedRecord`, `GraphAttachedRecordExpiry` |
| Provider timeout/unavailable/invalid response/not configured/busy or invalid provider input | Existing `InvalidQuery` with sanitized static reasons as internal labels only, including `ingestion provider timeout` and `ingestion provider unavailable`; proposed serving codes are listed below |
| Job admission full | Serving handler builds 503 `busy` directly; no fabricated `InvalidQuery` admission error |
| Cancellation | Existing `InvalidQuery("ingestion cancelled")` is an internal label; no cancellation wire code per [wire conventions section 2.2](serving-wire-conventions.md#22-one-reachable-family-oversize-deadline-cancel-busy) |
| Append/sync ambiguity, poisoned handle, I/O | Propagate `CommitIndeterminate`, `Poisoned`, `Io`; reopen and reconcile, never return complete |

These names exist in C13; reasons are proposed. `GraphKeyConflict` is the edge-key
conflict (C18) and is never used for receipts or anchors. Gap 1 names a public
`IdempotencyConflict` only as a proposal (Gap 1 section 2) whose name and mapping stay
provisional until public Rust API review; Gap 7 uses `InvalidQuery` reasons and adds no
public timeout, busy, conflict or ingestion variant by analogy. Serving authorization
uses its existing rejection layer, not a fabricated storage error.

Proposed serving mapping reuses the Gap 8 codes in the
[serving contract](serving-contract.md#gap-8-provider-policy)
and [wire conventions section 2](serving-wire-conventions.md#2-error-registry-c2-c9).
For each row below, the ingestion handler or provider adapter builds an `ApiError`
from the classified failure; static `InvalidQuery` reasons remain internal labels,
not wire codes. No new public `Error` variant or global `From<Error>` arm is added.

| Failure | REST code | gRPC |
|---|---|---|
| Provider timeout | 504 `provider_timeout` | DEADLINE_EXCEEDED |
| Provider unavailable | 503 `provider_unavailable` | UNAVAILABLE |
| Invalid provider response, including embedding shape/dimensions | 502 `provider_invalid_response` | UNAVAILABLE |
| Provider alias absent or disabled within the authorized binding | 409 `provider_not_configured` | FAILED_PRECONDITION |
| Provider admission full | 429 `provider_busy` | RESOURCE_EXHAUSTED |
| Invalid provider input | 422 `invalid_provider_input` | INVALID_ARGUMENT |
| Job admission full | 503 `busy` | UNAVAILABLE |

Shared mapper arms follow [wire conventions section 5.2](serving-wire-conventions.md#52-per-file-owner-and-sequence);
exact ingestion wire approval remains a section 10 gate.

## 7. Security and privacy

Serving derives tenant from the principal, never source text or an extractor. The
`tenant` identity component is the principal's tenant for tenanted principals; for
untenanted principals, admin/read-only tokens and embedded callers it is a label fixed
in the binding at provisioning, an operator-chosen value never implied by names or
source text. (A tenanted principal addresses `g` and reaches the stored graph
`t{tenant}__g`; other principals see stored names, `src/server/graph.rs:37-45`.)
Require graph-write plus vector-write grants for every touched collection,
graph binding and provider authorization. Graph-read alone already exposes entity
keys, `MENTIONS` surfaces, URIs and spans; only chunk text (the vector payload) also
needs its collection grant (`src/server/graph.rs:47-48`).
`collections:["*"]` does not grant graph scope (C12).
Use stored attachment collection identities exactly; do not qualify them twice.
Recheck tenant/binding generation under the write guard after provider work. Lineage
is checked at authorization and carried in resume tokens, never in keys (section 3).
Include tenant/binding in every canonical key and resume token; reject foreign
references before lookup to avoid existence probing. Embedded callers control the
file and trusted configuration; tenant tags are not a hostile-file-access sandbox.

Propose rejecting raw writes to reserved `G7*` objects, including creation,
modification and removal of receipts, shards and anchors, per option A of
[wire conventions, decision 14](serving-wire-conventions.md#6-owner-decisions-23).
Apply the rejection before mutation across raw graph/vector adapters and Gap 1
memory operations on a shared graph; do not add a `raw_access` grant. Only the
authorized ingestion path may write those reserved objects. Replay, delete and abort
still validate stored identity and back-references (receipt key/index,
source/revision keys, each owned vector record's metadata) before trusting any
`G7*` object. The ingestion graph and its collections must not be exposed to
untrusted raw writers or shared with Gap 1 memory scopes; `gap7-serving` owns
the proposed rejection gate. Embedded file access remains a trusted boundary.

Providers can see only the authorized chunk text and required pinned schema, not
unrelated evidence, tenant secrets or the whole database. Follow Gap 8's endpoint,
DNS/address allowlist, TLS, no-redirect and secret-reference policy (C15);
the old HTTP embedder's controls alone are insufficient (C3).
Do not infer egress permission from an environment key. Keep credentials outside
the database; disable cross-tenant provider caches and automatic body tracing.
Logs/errors contain only sanitized stage/category, opaque job ID and counts;
omit text, surfaces, URIs, raw tenant/source IDs, vectors, secrets and provider
bodies. Sanitize schema errors that could echo extractor-selected values.

Source text is **untrusted data**. LLM adapters delimit it as data, enforce a
structured allowlisted output schema and expose no tools or privileged retrieval.
Extracted names, URLs, prompts and relation values are never executed, evaluated
as code/query templates or trusted for authorization. Prompt instructions in text
cannot change provider selection, scope, limits or pipeline version. URI evidence
does not authorize fetching. Malicious output fails normal validation.

## 8. Independent oracle and acceptance tests

Qualification owns a Python reference using standard-library UTF-8, `hashlib`,
`struct` and explicit byte ordering, not Rust helpers, generated Rust results or
provider outputs as its expected answers. It independently implements both
chunkers, ASCII normalization, dictionary matching, identity framing, resolution,
ownership and dependency-group/batch ordering. Store hand-reviewed goldens for
input bytes, spans, canonical digests, normalized identities, ordered operations
and batch boundaries; provider fixtures are fixed accepted outputs/vector bits.
Test exact encoded-byte boundaries against a separately specified frame-size
calculation; compare the engine encoder as the implementation, not the oracle.

| Requirement | Independent acceptance |
|---|---|
| ING-001 | Same goldens across Windows/Linux/macOS; LF/CRLF, BOM, emoji, combining marks, Turkish I, invalid UTF-8, whitespace, exact overlap and EOF |
| ING-002/007 | Exactly 4,096 operations / 16,777,216 bytes accepted when valid; +1 rejected; include receipt shards, vector dimensions and reclaim bytes; a commit-time `BatchTooLarge` appends nothing and keeps any partial job; all source/chunk/extraction/plan limits tested at and above boundary |
| ING-003/006 | No-network default with trap provider; unauthorized wildcard/tenant/provider references fail before dispatch; DNS/redirect/TLS/secret and reauthorization fixtures |
| ING-004/006 | Proposed reserved-object gate rejects raw creation, modification or removal of `G7*` receipts, shards and anchors through graph/vector adapters and shared-graph memory operations; ingestion receipt replay still validates identities and back-references |
| ING-004 | Replay before providers; conflicting request/plan digest, wrong binding generation, shuffled outputs, duplicate calls and source CAS races |
| ING-005/009 | Exact evidence spans and paths; shared anchor across two sources, distinct relation evidence, contradiction preservation; replace/delete removes only owned objects |
| ING-008/012 | Crash before/after each frame and after append before acknowledgement; reopen, compare receipts and resume exact plan; missing bundle fails resume but not abort; status stays Partial until the final batch receipt; raw-writer interference (changed or removed object, unrelated vector at a planned record ID, anchor create race) appends nothing and keeps the partial job; abort crash between frames, abort without the bundle, abort never touching another source's objects or retained shared anchors, replay returns `Aborted` |
| ING-010 | Attached expiry/deletion rejected; detach/vector-delete ordering; graph TTL does not imply vector erasure; tombstone replay/purge and bounded delete resume |
| ING-011 | Reopen, encryption, compaction, backup/restore and directory checkpoints preserve receipt/progress/ownership; use explicit Full/sync cases for durable acknowledgement; a lineage change (compaction, upgrade, restore) with a Partial job fails closed with `ingestion lineage changed`, sequence renumbering changes no key or digest, and replay returns the stored result with the current `receipt_revision` |

Inject failures at validation's final operation, extractor response decode,
embedding dimension check, receipt creation, append, sync, and every boundary
between cleanup/addition/abort batches. Assert no unreceipted vector/graph changes;
an indeterminate frame is either wholly present or absent after reopen.
Incremental tests must observe partial data through raw reads, so a hidden
document-wide atomicity assumption cannot pass unnoticed.

Mutation list: change `<=` to `<` at chunk/operation/byte ceilings; split a UTF-8
scalar; advance by `B` ignoring overlap; normalize CRLF or Unicode; omit tenant
or pipeline from identity; use unordered-map iteration; swap relation endpoints;
accept a foreign local ID/span; merge conflicting attributes; omit receipt bytes;
commit vectors separately; move receipt to KV; update head before progress;
skip a batch; reuse different model output on resume; detach after vector delete;
erase shared anchors; retry an indeterminate append without reopening; allow
`collections:["*"]` to imply graph access; log provider bodies; make abort need the
lost bundle or touch another source's objects; hash lineage into a key; patch a
receipt; overwrite an unrelated vector at a planned record ID; omit an anchor create
without re-reading it. Each mutation
must fail a named golden, failure-injection or authorization test.

## 9. Delivery slices and independent gates

Paths below are proposed exclusive ownership, not files created by this design.
Later slices start only after predecessor acceptance; qualification defines its
oracle early but runs its final gate after all enabled surfaces.

| Slice / dependencies | Files owned | Required tests and independent acceptance checklist |
|---|---|---|
| `gap7-core` / owner approves this model | `src/ingestion.rs`, `src/ingestion/` except `providers.rs`; minimal module export in `src/lib.rs`; `tests/ingestion_core.rs`; `docs/GRAPH.md` ingestion section; `site/api.html` rows for the new public items | Local chunkers/extractors, typed embedded prepare/commit, receipts, replacement/delete, explicit incremental mode and abort/cleanup of a Partial job (ING-012). Checklist: independent golden comparison; exact bounds; no provider calls on replay or abort; crash/reopen; mixed-frame and attached-record invariants; `python tests/api_reference.py` passes (every new `pub` item has a `site/api.html` row; `src/server*` and `src/bin/*` are excluded). No storage opcode, serving or dependency changes without separate approval |
| `gap7-providers` / **BLOCKED**: accepted core and a Gap 8 amendment adding an extraction-provider kind (section 10) | `src/ingestion/providers.rs`; ingestion-only fixtures in `tests/ingestion_providers.rs`; ingestion provider documentation; `site/api.html` rows for any new public items | Cannot start until Gap 8 accepts that amendment and names its slice; then consume its contracts and do not implement a competing registry. Checklist: explicit egress only; alias/tenant/binding checks; response/deadline/concurrency caps; prompt-injection fixtures; no fallback or sensitive logs; `python tests/api_reference.py` passes. Shared registry edits remain owned by Gap 8 |
| `gap7-serving` / step 9 of [wire conventions section 5.1](serving-wire-conventions.md#51-order), accepted core (and providers, only if enabled) and owner approval of exact public wire **before work starts** | New `src/server/ingestion.rs`; narrowly coordinated dispatch in `src/server.rs` and `src/server/grpc.rs`; `src/openapi_graph.json` for any `/v1/graphs` paths and `src/openapi.json` for components (and any non-graph paths), per [wire conventions section 5.2](serving-wire-conventions.md#52-per-file-owner-and-sequence); `proto/vlite/v1/vlite.proto`, relevant client/binding/tool adapters and API documentation only as enumerated in the approved wire inventory | Write exact request/result/error/status/resume schemas first, including numeric encoding and transport body caps. Checklist: approved surface/file/tag inventory; auth/quota/cancellation parity; section 6 provider/admission error mappings; proposed raw-write rejection for reserved `G7*` objects; receipt replay across adapters; transport limits fail explicitly; no unapproved REST/gRPC/MCP/CLI/Python additions |
| `gap7-qualification` / accepted enabled slices | `tests/ingestion_reference.py`, `tests/fixtures/ingestion/`, `tests/ingestion_qualification.rs`; this design's acceptance/status updates | Own independent Python oracle and hand-reviewed goldens. Checklist: cross-platform digest parity; every mutation killed; frame/crash/replay and maintenance matrix; tenant/egress/redaction tests; publish pass/fail evidence before release |

`gap7-providers` is blocked. Gap 8 as written defines embedding and reranker
providers only and has no extraction-provider kind (C15); `gap8-provider-registry` is
a placeholder name used here, not a slice the Gap 8 document defines. Gap 8 must first
accept an amendment adding that kind (graph/tenant binding, request/response schema,
the existing 1 MiB request / 4 MiB response bounds) and name its slice; until then
Gap 7 ships only local in-process extractors. The slice graph is acyclic: `gap7-core`
precedes `gap7-providers` and `gap7-serving`, and `gap7-qualification` follows every
enabled slice. Only `gap7-providers` waits on a deliverable of the Gaps 8 and 12 row
of `docs/projects/roadmap/next-major.md` (line 23); `gap7-core` does not. Gap 1's
receipt design is prior art for the receipt pattern, not a callable memory core:
`gap7-core` must not depend on Gap 1 crate-private internals or candidate-only names.

## 10. Owner decisions and open items

| Decision | Options | Recommendation / gate |
|---|---|---|
| Commit unit | Reject oversized documents; explicit visible incremental mode; new staged-publication storage/read protocol | Default single-frame atomicity, opt-in visible incremental mode as specified; no hidden activation claim |
| Identity/merge | Exact ASCII normalization; Unicode normalization; semantic merge | `ascii-key-v1`, immutable shared anchors and source-local assertions; later normalization requires new pipeline version |
| Local extraction | Dictionary rules; heuristic NER; required LLM | Deterministic dictionary built-ins and caller-supplied in-process extractors; no external default; external providers wait on the extraction-provider decision below |
| Bounds/deadlines | Proposed ceilings; different explicit ceilings | Approve section 6 plus chunk/extractor limits as one versioned policy, with operators allowed to lower |
| Replacement/history | Delete old content; retain versions; temporal integration | Delete old source-owned text/evidence, retain shared anchors and compact receipts; history is separate scope |
| Retention | Implicit TTL/eviction; explicit delete and receipt purge; Gap 1-style expiry window | Explicit guarded deletion; receipts, result shards and tombstones kept until explicit purge, no growth bound claimed; purge mechanics, size accounting and any retention window are decided before any purge code and reconciled with Gap 1 (receipt expiry only by explicit policy) and Gap 4; acknowledge log/backup erasure limits |
| Error API | `InvalidQuery` static reasons; new typed ingestion/provider errors | `InvalidQuery` reasons first (section 6); a typed ingestion error or **any new public `Error` variant requires separate explicit approval** and a `site/api.html` row |
| Reserved objects and raw writers | Reject raw writes to `G7*`; allow an explicit `raw_access` grant; accept forgeable receipts | Propose option A: reject raw writes to reserved `G7*` objects before mutation, as specified in section 7; retain stored-identity validation. Wire conventions, decision 14 (proposed). |
| Serving | Reuse raw graph batch wire; new ingestion wire; embedded only | Embedded first; exact transport schemas, endpoints/RPC tags, resume payloads and errors need separate approval before `gap7-serving` |
| Prepared-plan custody | Caller retains bundle; encrypted durable job spool | Caller-retained bounded bundle initially; losing it blocks resume, and recovery is abort from the ownership shards (ING-012) then re-ingest, never new inference |
| Record-ID allocation | Planner-chosen candidate IDs absence-checked per frame; engine allocator; derived IDs | Planner-chosen IDs frozen in the plan and required absent under every frame's exclusive borrow (`ingestion record id conflict`); an engine allocator or derived IDs need separate approval (C16) |
| Extraction-provider ownership | Gap 8 amendment; a Gap 7-owned registry; local only | Gap 8 amendment first, since Gap 8 as written has no extraction kind (C15); blocks `gap7-providers`; local in-process extractors only until accepted; no competing Gap 7 registry |
| Receipted re-key | Allow re-keying a Partial job after a lineage change; fail closed | Fail closed (`ingestion lineage changed`); the Partial job can only be aborted; a re-key needs a separate design |

Open: the Gap 8 extraction-provider amendment and its registry API/generation hooks;
independent frame-size reference review; how public transports carry a bounded
prepared bundle without exceeding their approved request caps; receipt purge
mechanics and size accounting, retention versus Gap 1 and Gap 4, and physical
erasure runbooks.
These are delivery gates, not permission to guess a wire, relax isolation or add
a background service. No code, API, dependency or format change accompanies this
design proposal.
