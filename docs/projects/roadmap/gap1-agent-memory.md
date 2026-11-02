# Gap 1: agent memory

Status: the crate-private A1 core (remember, receipts and one graph batch) and A2 (bounded
exact-ID forget and graph-TTL expiry) are integrated after independent acceptance; they are
unreleased, not activated and have no public API. Recall, consolidation, resumable erasure and
serving are not implemented. Open decisions and serving approval
were granted 2026-11-02 (section 9). Provisional Rust names only; no public memory API or wire
approval is claimed.
Scope: embedded remember, recall, consolidate and forget over existing storage.
Dependencies: Gap 2 key-value, Gap 3 graph TTL and Gap 4 temporal validity.
Evidence below refers to checkout base `c92c8c0`; proposed names are not existing APIs.

## Constraints and evidence

The roadmap's Gap 1 row requires all four actions after Gaps 2-4.
Source: `docs/projects/roadmap/next-major.md:21`.
The product gap calls for sessions, users and fact updates as a thin layer.
Source: `docs/projects/product-gaps/product-gaps.md:46-49`.
Rust comes first; `src/openapi.json` changes only in slice D, from a schema written here and reviewed first.
Preserve format compatibility, exact-search results and major-release benchmark gates.
Source: `docs/projects/roadmap/next-major.md:117-125`.
No REST, gRPC, MCP, CLI, bindings or OpenAPI changes belong to this slice.
Gap 4 section 6 supplies validity reads, revision-checked supersession and deletion.
Source: `docs/projects/roadmap/gap4-temporal-graph.md:202-215`.

| Current constraint | File:line evidence | Design consequence |
|---|---|---|
| Ordinary vector batches target one collection | `src/lib.rs:2059-2081`; `src/collection.rs:494-495` | Do not describe them as database-wide transactions |
| Only graph-enabled files commit cross-collection vector operations plus graph in one frame | `src/property_graph/mod.rs:696-721`; `docs/STORAGE-FEATURES.md:193-202` | Require graph enablement for the complete memory contract |
| Graph batches have 4,096 user operations and a 16 MiB body limit | `src/property_graph/mod.rs:374-379`; `src/property_graph/mod.rs:568-598` | Count receipt, revision guards and temporal operations; never split silently |
| Attached vectors cannot expire or be deleted before detachment | `docs/GRAPH.md:47-53`; `docs/GRAPH.md:90-93` | Memory TTL hides graph owners; evidence cleanup is separate |
| Read results expose sequence, completeness and budgets | `docs/GRAPH.md:95-124` | Preserve diagnostics instead of returning success-shaped empty results |
| Hybrid retrieval filters and revalidates pivots before attachment expansion | `src/property_graph/retrieve.rs:370-438` | Reuse this path, extending scope/validity checks |
| Key-value namespaces, ordered prefixes, expiry and revision fencing exist | `docs/STORAGE-FEATURES.md:257-288` | Use KV for independent coordination, not an assumed mixed transaction |
| KV prefix listing scans the whole collection; pages cap at 10,000 | `docs/STORAGE-FEATURES.md:276-281` | A page limit alone does not bound scan work |
| Exact rerank validates and sorts by distance then ID | `src/rerank.rs:21-51` | Candidate-only precision repair; deterministic ties |
| MMR uses cosine, finite lambda in [0,1], and ID ties | `src/rerank.rs:54-104` | Optional diversity, not temporal or semantic conflict resolution |
| Embedded rerank fetches vectors for at most 4,096 hits | `src/rerank.rs:106-132` | Bound fetched vectors and pairwise work separately |
| REST exact/MMR rejects hybrid and caps candidates at 1,000 | `src/server.rs:2647-2671`; `src/server.rs:3032-3054` | Do not claim an existing hybrid memory rerank route |
| gRPC exact/MMR rejects hybrid and caps candidates at 4,096 | `src/server/grpc.rs:1081-1109`; `src/server/grpc.rs:1135-1153` | Serving limits differ today; later parity needs review |
| Periodic durability can lose acknowledged commits on OS failure | `docs/STORAGE-FEATURES.md:242-252` | Atomic visibility is not unconditional power-loss durability |
| Write/sync failure can be indeterminate and poisons the handle | `src/lib.rs:2053-2058` | Reopen and inspect the receipt before retry |
| Embedded file access has no server tenant enforcement | `docs/GRAPH.md:159-172` | Scope prevents accidental mixing, not hostile access to the file |

## 1. Model, identity and scope

Use one explicitly provisioned memory graph and evidence collection binding.
Provisioning is a separate, retryable setup step, not part of remember atomicity.
Do not allocate a graph or collection per session; there are only 64 graph slots.
Keep the single-file deployment; the same logical contract may use P2 directories.
No external database, model service, scheduler or background worker is required.

| Kind | Representation | Meaning |
|---|---|---|
| Episodic | Immutable memory node plus optional attached evidence record | An observation, message, task result or caller-provided summary |
| Fact | Stable subject and value anchors joined by a Gap 4 temporal fact series | A typed assertion in a caller-defined fact slot |
| Entity | Stable keyed node with nonhistorical descriptive properties | Identity anchor; changing historical attributes belong on fact edges |
| Relation | Keyed temporal series between stable entity anchors | A typed relationship, possibly nonexclusive |

Every memory has a stable opaque ID, schema version, kind and exact scope tuple.
Scope is `{ tenant, agent, user: Option<String>, session: Option<String> }`.
Require tenant and agent; require user when session is set in the first version.
Absent user/session means explicitly shared within that tenant/agent, not a wildcard.
Remember writes exactly one scope; cross-scope copies require a separate call.
Recall names a bounded allowlist of exact scopes, with no implicit parent fallback.
Reject references, attachments, merges and supersessions crossing their scope.
Entity anchors are scope-local; shared identity never grants shared evidence access.
Encode tuples canonically with length framing, not ambiguous slash concatenation.
Use bounded opaque keys with a versioned digest; verify the full tuple on lookup.
Reject a digest collision rather than aliasing another scope or identity.
Do not concatenate arbitrary tenant identifiers into existing physical name syntax.
The caller selects bindings; serving will derive tenant scope from the principal.

Store provenance: caller source ID, source kind, content digest, optional URI,
evidence record IDs, contributing memory IDs/revisions and transformation ID.
URIs are inert references: do not fetch, execute or treat their contents as instructions.
Require `created_at_ms`, `observed_at_ms`, `updated_at_ms` and optional expiry.
Creation/update times come from one captured engine clock; observation is caller data.
Importance is caller-supplied finite f64 in [0,1], default 0.5; it is not confidence.
Optional caller confidence is separate and does not prove an assertion true.
Use signed i64 UTC epoch milliseconds for observation and validity, as Gap 4 does.
Expiry uses the existing u64 clock; checked conversions must reject overflow.
Reject nonfinite scores, invalid intervals and future observation times for ranking
unless an explicit future-observation policy allows them; never wrap arithmetic.

Fact-slot identity is `(scope, subject_id, predicate, caller_slot_key)`.
The caller declares a slot exclusive or multivalued when provisioning its schema.
Exclusive means no overlapping accepted values in that slot; touching bounds are legal.
Multivalued relations permit parallel values but each series obeys Gap 4 overlap rules.
A changed destination creates a different series, not a changed endpoint.
Versioned evidence records keep old fact text/embedding rather than overwriting it.
Segment provenance points to those versions; anchors remain today's retained nodes.
Temporal slots and their mutation rules depend on Gap 4 implementation, not metadata
properties opportunistically named `valid_from` or `valid_to`.

## 2. Embedded API and remember

Propose borrowed Rust handles, following `GraphRef` / `GraphMut` ownership:

```rust
// Proposed signatures; types and error names require public Rust API review.
memory_mut.remember(RememberRequest) -> Result<RememberResult>;
memory.recall(RecallRequest) -> Result<RecallResult>;
memory_mut.consolidate(ConsolidateRequest) -> Result<ConsolidateResult>;
memory_mut.forget(ForgetRequest) -> Result<ForgetResult>;
```

Requests carry typed scope, limits and expected revisions, not arbitrary query text.
Embeddings and extraction are supplied by the caller; text-only evidence is allowed.
Do not invent a zero vector for absent embeddings; use existing optional-vector storage.
Remember accepts one bounded bundle of anchors, observations, assertions and evidence.
Return created/merged/no-op IDs, affected revisions, receipt ID and commit sequence.
Separate validation/conflict errors from indeterminate storage errors.

Require an idempotency key per mutation, scoped to operation and exact scope.
Compute a versioned canonical request digest including content, vector, metadata,
validity, provenance, policy and expected revisions; exclude execution deadlines.
Persist a keyed graph receipt with digest and stable result identifiers in the same
frame as all memory changes. A separate KV put is not atomic with that frame.
On matching key/digest, return the original outcome without merging or changing TTL.
On matching key with different digest, return `IdempotencyConflict`.
Serialize planning under the mutable database borrow; condition receipt creation
on absence, and guard every changed existing object by its expected revision.
Recover receipt sequence from its committed revision; do not predict next sequence.
Receipts may expire only under an explicit retention policy and advertised retry window.
After expiry, the key may execute again; memory IDs/content identity still prevent
accidental exact duplicates, but do not promise indefinite replay of the old result.
Reopen after an indeterminate failure: receipt present means the whole frame committed;
receipt absent means a fresh retry may proceed on a healthy handle.

Distinguish idempotency, exact deduplication and semantic consolidation.
Exact dedup uses canonical scope/kind/content digest plus caller source identity.
Repeated source/content is a no-op; a new independent source may append provenance
only through an explicit merge policy with bounded lists and expected revisions.
Do not silently merge paraphrases, overwrite entity properties, raise importance,
extend retention or resolve contradictory values because vectors are similar.
Exclusive-slot conflicts return competing IDs/bounds and require explicit correction.
Accept disagreement as separate disputed assertions only when the caller selects that
policy; recall must label it, not choose an unmarked truth.
Plan exact dedup, slot checks, receipts, evidence and attachments before append.
Validation failure changes nothing, including computed TTL cleanup.
Use one graph frame for the complete bundle, even when evidence spans collections.
No atomic multi-file operation, remote embedding call or KV lease acquisition is implied.
Oversized bundles fail before append; callers may explicitly split independent memories.
Offer `Durability::Full` or `sync()` for stronger acknowledgement requirements,
without altering the database's configured durability behind the caller's back.

The integrated A1 core is deliberately narrower than this proposal. Its
crate-private `MemoryRef` / `MemoryMut` bindings require an existing graph and can
validate an optional existing evidence collection; it currently writes only entity
and episodic memory nodes, source/provenance properties, references and receipts.
It does not attach vector records or provide public names, public errors, settings,
recall, forget, expiry, facts or serving surfaces. Names and error mapping remain
provisional until public Rust API review.

The A1 limits are exact: a request has 1-4,096 items; opaque scope, identity,
source and idempotency keys are at most 256 UTF-8 bytes; provenance has at most 64
entries and its length-prefixed UTF-8 encoding is at most 4,096 bytes. Each
source/kind/URI/transformation field is at most 256 bytes. The separately encoded
`sources` property is also at most 4,096 bytes. A canonical request and the
serialized graph frame are each limited to 16 MiB. The single graph batch is
limited to 4,096 operations, counting receipt and outcome-chunk nodes, large
content-chunk nodes and HAS_CONTENT edges; each merge also charges one logical
write and one expected-revision guard even when same-target graph upserts coalesce.
Individual stored string properties are at most 4,096 bytes. Large memory content
and receipt outcomes use separate keyed graph nodes, with each chunk at most 4,096
UTF-8 bytes, in the same frame. A1 does not expire receipts.
The graph engine additionally caps each node at 64 property fields and 65,536
encoded property bytes; the receipt and each outcome chunk remain separate small
nodes so receipt outcomes cannot overflow a single node's metadata.

Receipt creation is a keyed graph-node create in the same `GraphMut::write_batch`
as all memory operations. Key absence is enforced by the graph create operation;
changed existing memory nodes use expected graph revisions. Matching key/digest
replay returns the stored outcome and committed receipt revision; digest mismatch
conflicts. Validation and budget failures are preflighted before the batch. These
are candidate implementation notes, not a released contract or durability claim.
Graph batch operations are applied in order and observe prior operations in the
same batch. The candidate therefore plans against virtual in-request revisions,
coalesces repeated updates to an existing node into one upsert guarded by its
original graph revision, and updates a node created earlier in the request in its
original create operation. Its logical operation budget still charges every merge.

## 3. Recall, time selectors and explanation

Recall accepts vector and/or text, typed metadata filters, exact scopes, kinds,
optional entity seeds, explicit `as_of_ms`, ranking policy and budgets.
Combine dense/sparse/text legs through existing hybrid/RRF machinery.
Conjoin mandatory scope/kind/retention filters with caller filters; never let callers
replace the mandatory predicates or broaden the scope through an OR expression.
Capture a borrowed committed view, validity T and TTL clock C once.
Use Gap 4's `as_of(T)` for temporal edges and `with_clock(C)` for TTL independently.
Without explicit T, use checked captured now; deadlines use monotonic elapsed time.
No wire route or current graph query syntax is being extended by this proposal.

Seed graph expansion from filtered attachment pivots and explicit in-scope entities.
Apply scope and TTL before discovery; apply validity and fact status before traversing
each edge. Apply caller return filters separately, without enabling cross-scope paths.
Revalidate evidence ownership, scope, revision and retention before scoring/output.
Expired memory owners hide attached evidence even though vector records cannot expire.
The planner must apply that authoritative owner check to every candidate, not only
graph-returned hits; duplicated expiry metadata is an optimization, not authority.
Exclude superseded facts at T, but preserve their evidence for retained earlier intervals.
An as-of request does not reconstruct old anchor metadata, overwritten corrections,
deleted records, past ranking settings or an old ANN index.
Historical candidate discovery can miss evidence; report this limitation explicitly.
Current top-N pivots plus temporal filtering do not guarantee full historical recall.

Rank the eligible bounded union, never the entire database by implication.
Retain hybrid leg ranks/scores; do not treat RRF scores as cosine similarities.
Use rank-derived relevance `R = 1 / rank` in the eligible fused candidate ordering.
For candidate observation time O, choose `age = max(0, T - O)` with checked arithmetic.
Recency is `D = 1 / (1 + age / half_life_ms)`; require positive finite half-life.
Importance is the stored caller value I; calculate `S = wR*R + wD*D + wI*I`.
Weights are finite, nonnegative and sum to one; default relevance-only `(1,0,0)`.
Expanded-only candidates receive R=0; their graph path contribution is explained,
not secretly converted into a similarity score. This policy is versioned.
Sort descending S, then canonical scope, memory ID and matching segment bounds.
Sort provenance and paths canonically; stable ties hold for the same view/inputs,
not for different approximate candidate sets or platform floating-point behavior.

Optional exact rerank recomputes distances on vector candidates only.
Reuse `ExactReranker`; it cannot restore candidates omitted by ANN or graph budgets.
Optional MMR uses existing cosine semantics and ID tie-breaking on embedded candidates.
Do not pass text-only memories or fused scores as invented cosine vectors/distances.
First slice keeps MMR a separate vector-only selection mode, not a blended recency
objective; unsupported hybrid/MMR combinations fail rather than discard ranking weights.
Custom caller rerankers must return unique in-set IDs and finite scores; validate output.
No built-in LLM, cross-encoder provider or network request is part of recall.

Return each memory with matched segment bounds, provenance, evidence snippets,
contradiction status, paths, raw leg ranks, distance/BM25 where present, R/D/I/S,
ranking version and selection mode. Projection bounds snippets and sensitive fields.
Return effective scopes, T, C, sequence, candidates examined, edges charged,
evidence bytes fetched, complete/truncation reasons and candidate-search mode.
Distinguish exact candidate enumeration, approximate search and bounded expansion;
`complete` never means exhaustive semantic or historical truth.
An empty complete result means no eligible memory in the declared search plan.
Budget exhaustion may return a marked partial result only when explicitly requested;
otherwise return a typed budget error. Cancellation is not an empty successful answer.
No recall paging initially; later cursors must bind scope, policy, T/C and view sequence.

## 4. Consolidation, correction and contradictions

Consolidate accepts caller-produced summary text/embedding, exact input IDs/revisions,
provenance, output scope, validity and an explicit disposition for each input.
Default disposition retains source episodes; consolidation never implies forgetting.
No automatic clustering, extraction, LLM generation or periodic agent loop is included.
Allow a dry-run plan reporting conflicts and operation/byte estimates, not a reservation.
Revalidate the complete input set and retention at commit; stale inputs fail atomically.
Store the summary as an episodic derived memory with transformation identity and
bounded source references; one receipt guards summary plus intended source changes.
Exact retry returns that summary, not another summary of the same inputs.

Correct facts through Gap 4 revision-checked replace/remove/close operations.
Supersession closes the old series segment at c and creates/appends replacement at c
in one graph batch, including replacement evidence and exclusive-slot guards.
Require c inside the old interval unless using an explicit interval correction;
adopt Gap 4's exact-end no-op and half-open boundary rules.
Guard a stable slot node revision as well as touched series so concurrent writers
cannot install overlapping values on different edges.
Past answers reflect the currently retained correction, not past beliefs.
Do not fabricate a revision timeline from `updated_at_ms` or commit sequence.

Contradiction detection is structural for exclusive slots and caller-supplied otherwise.
Different predicates, units or entity identities are not automatically contradictory.
Disputed assertions remain separately addressable with provenance and status.
Only an explicit caller resolution may retract, supersede or select an accepted value.
Derived summaries depending on corrected/deleted inputs are marked stale when discovered;
bounded invalidation must report remaining work, never claim a complete cascade.

## 5. Forget, retention and erasure

Forget names exact IDs, a fact series, entity, session or exact scope plus expected
revisions and idempotency key. Offer retention expiry or logical deletion explicitly.
Closing validity is neither deletion nor privacy erasure.
Memory TTL uses graph owner/series deadlines; attached evidence has no record expiry.
At C equal to expiry, exclude the memory and its evidence regardless of T.
Graph TTL hides incident edges and later reclaims graph state; it does not erase
vector payloads merely because their owner expired or was reclaimed.
Run an explicit bounded orphan-evidence cleanup plan; do not delete evidence shared
with other retained memories. Evidence ownership/reference checks are mandatory.
Sources intentionally copied to other scopes need independent deletion authorization.

Small explicit forget detaches evidence, deletes unshared records, deletes the entire
fact series/history and removes unneeded anchors in one graph frame with a receipt.
Respect all attachments, including ones outside the memory layer; a conflicting
attachment prevents deletion, rather than being detached without authorization.
Forget entity includes bounded incident-edge handling; never leave dangling edges.
The accepted private A2 slice implements bounded exact-ID forget and graph-TTL expiry only;
oversized erasure is rejected, not converted into a resumable job. Its acceptance
covered shared/unshared evidence, outside-attachment, incident-edge, expiry-boundary,
receipt-replay and reopen cases, plus mutation testing.
Oversized session/entity/scope erasure becomes a resumable, explicitly non-atomic job.
First commit a scope/entity deletion barrier; every memory read/write checks it.
Subsequent revision-guarded chunks detach/delete and advance a persisted graph job.
Each chunk and progress update share a frame; retries use deterministic chunk receipts.
Return pending counts or unknown remainder, resume token and logical visibility state.
Do not promise all-or-nothing physical cleanup across chunks.
Independent KV namespace/prefix entries can hold worker claims with expiry/CAS.
An expired lease is not sufficient fencing: validate worker generation on the graph
job and its expected revision in every chunk frame; stale workers cannot commit.
Do not use unbounded KV prefix scans as the job planner or an erasure completion proof.

Deletion removes live state, not necessarily old bytes in append logs, checkpoint
images, backups, exports, replicas, feeds, application caches or retained summaries.
Compaction may rewrite live data but is not secure overwrite on disks or SSDs.
Encryption protects storage but does not provide per-memory cryptographic erasure.
Restore can resurrect forgotten content unless the operator reapplies deletion policy.
Maintain deletion manifests for restore/consumer workflows under an explicit retention
policy; manifests themselves must not retain forgotten content or sensitive source URIs.
Do not declare backup/history erasure complete from a successful engine delete.
Graph TTL feeds currently omit deadlines/reclaims; no replication completeness claim.
Caller-owned consumers must acknowledge purge independently; the embedded engine has
no authority to delete remote copies, logs, process memory or backups.

## 6. Bounds and storage

| Proposed memory limit | Default / hard ceiling | Enforcement |
|---|---|---|
| Exact recall scopes | 1 / 16 | Reject excess; no automatic tenant-wide scan |
| Recall output memories | 20 / 100 | Apply after eligibility and ranking |
| Candidate union | 200 / 1,000 | Across all legs/seeds; charge duplicates examined |
| Exact source scan | 100,000 records | Preflight collection size; reject before scan |
| Vector components fetched | 1,000,000 / 4,000,000 | Checked candidate-count times dimension bound |
| Evidence bytes fetched/output | 1 MiB / 4 MiB | Charge before allocation; explicit projection/truncation |
| Provenance/consolidation inputs | 64 / 256 | Reject overflow; no silent provenance loss |
| Temporal segments per series | 256 | Gap 4 cap; corrections consume user operations |
| MMR similarity component work | 10,000,000 / 100,000,000 | Charge pair comparisons times dimensions |
| Mutation user operations/body | 4,096 / 16 MiB | Existing frame rules, including internal reclaim bytes |
| Graph expansion/deadline | Existing defaults/ceilings | `docs/GRAPH.md:100-112`; every examined edge charged |

These are proposed safety budgets, not latency, throughput or memory-use claims.
Candidate/output caps do not bound ANN construction, filter scans or BM25 indexing.
Initially require ready indexes for approximate recall; no lazy build in a read call.
Planner work accounting/deadlines must cover source scans, fused legs, attachment
checks and scoring, not only graph traversal; reject unsupported bounded plans.
Custom rerankers execute caller code; the engine cannot preempt arbitrary Rust code.
Require a cooperative cancellation/budget contract or exclude them from bounded mode.
Reserved graph schema, receipt encoding and ranking policy carry explicit versions.
No new memory WAL format is proposed; temporal storage uses Gap 4's required capability.
Legacy files stay untouched; enable graph only through the existing copy converter.
Reopen, encrypted maintenance and P2 checkpoints must retain receipt/slot/job revisions.
Compaction/restore may change revision tokens; resume jobs only after reconciliation.
No benchmark campaign or performance qualification is part of this design.

## 7. Competitor comparison

Comparison uses the repository's product-gap research and linked primary documentation
as design precedents, not parity claims. First-party references were checked during
proposal review; recheck product details before choosing implementation dependencies.
Mem0's hosted benchmark claims are distinct from its open-source SDK, whose default
self-hosted configuration also depends on OpenAI and Qdrant.

| Product / primary reference | Copy for an embedded engine | Do not copy |
|---|---|---|
| [Mem0](https://github.com/mem0ai/mem0) | Simple scoped add/search/update/delete vocabulary; make provenance and explicit updates visible | Model-mediated extraction/dedup as an implicit storage write; hosted dependencies or benchmark claims |
| [Zep/Graphiti](https://github.com/getzep/graphiti) | Distinguish episodes, entity anchors and valid-time facts; retained invalidation and provenance | Full bitemporal scope, automatic contradiction inference or an external graph-service requirement |
| [Letta memory](https://docs.letta.com/v1-sdk/concepts/stateful-agents) | Separate retained recall material from the agent's selected working context; let callers manage importance | Agent runtime, prompt editing, token-budget autonomy or a built-in model loop |
| [LangGraph memory](https://docs.langchain.com/oss/python/concepts/memory.md) / [LangMem](https://github.com/langchain-ai/langmem) | Namespace/session separation, store primitives and caller-controlled consolidation | Framework checkpointer semantics as a database transaction guarantee; mandatory background model jobs |
| [Cognee](https://github.com/topoteretes/cognee) | Evidence-linked graph/vector retrieval and inspectable transformation provenance | End-to-end extraction pipelines, infrastructure orchestration or automatic external providers |

## 8. Acceptance matrix

| Case | Required result / slice |
|---|---|
| Scope encoding/isolation | Ambiguous strings, digest collisions, OR filters and cross-scope references reject; expansion/evidence cannot leak / A,B |
| Kinds and timestamps | All four kinds, vector-free evidence, finite importance, overflow and validity boundaries exercised / A,B |
| Idempotency | Same digest returns original IDs; changed digest conflicts; retry after TTL follows advertised window / A |
| Dedup/merge | No semantic merge; provenance merge is bounded and revision-checked; independent scopes stay distinct / A |
| Atomicity/failure | Final-op failure changes no vectors/graph/receipt/TTL cleanup; sync ambiguity reconciles on reopen / A,A2 |
| Fact exclusivity | Concurrent different destinations cannot overlap; multivalued slots remain legal / A,C |
| Hybrid recall | Mandatory filters hold before discovery and output; unmapped, expired and unauthorized evidence excluded / B |
| Time selectors | Fixed T/C and sequence; from/to boundary, corrected past, TTL equality and post-purge non-resurrection / B,C |
| Ranking/explanation | Formula fixture, stable ties, raw leg diagnostics and graph-only R=0; exact rerank remains candidate-only / B |
| Diversity/providers | Existing cosine MMR ties; hybrid/MMR rejects; duplicate/nonfinite/out-of-set custom results reject / B |
| Work bounds | Caps at/either side, high-degree invalid edges, scan preflight, MMR components and cancellation charged / B |
| Consolidation | Caller summary provenance, stale inputs, retry, source retention and atomic close/replacement / C |
| Forget/TTL | Small forget is one framed receipt operation; detach before vector delete, shared evidence retained, exact expiry boundary hides memory/evidence / A2; series and resumable erasure remain / C |
| Large erasure | Barrier hides pending scope; chunk retry/crash, stale fence and restore reconciliation; no atomic-job claim / C |
| Maintenance | Reopen, encryption, backup, compaction and directory checkpoint preserve live receipts, slots and jobs / A,C |
| Serving gate | No schema drift now; later authorization, feature refusals and diagnostics agree on every approved surface / D |

Use small deterministic fixtures and failure injection, not performance assertions.
An independent interval/ranking oracle supplements but does not replace engine tests.

### Private ranking reference

The independently reviewed `tests/memory_ranking_oracle.py` and
`tests/fixtures/agent_memory/ranking_golden.json` cover ranking over an already
eligible, bounded union. CI runs normal and optimized Python self-tests, including
default future-observation refusal, dense ranks, explicit decay half-life, scoped
identity/segment ties, malformed inputs and intended-reason mutation checks.
Exact rational scores, fixture limits and request/error names are labelled local
assumptions, not Rust/f64 parity or approved public contracts. Retrieval, ownership,
TTL, temporal discovery, rerankers and the Rust recall implementation remain open.

### Private write reference

The independently reviewed `tests/agent_memory_oracle.py` and
`tests/fixtures/agent_memory/golden.json` are a bounded independent reference for slice
A remember/forget: scope framing, receipts and idempotency, exact dedup, provenance,
the 64-entry and 4,096-byte encoded provenance limits, expiry and operation budgets.
Every rejected golden step declares one repairable
defect; the self-test repairs it and checks injected second defects, so no golden
depends on validation precedence. CI runs normal and optimized self-tests.
Unspecified behavior is labelled, e.g. `fixture: duplicate-bundle-ids-reject` and
`fixture: empty-bundle-rejects`. Field/error names are fixture APIs, not approved
public contracts. Rust replays every representable golden; the remaining fixtures
require an injectable hash collision or input values/types not representable by the
typed Rust request.
The oracle and Rust use one physical operation model: receipt and outcome chunk nodes,
content chunk nodes and HAS_CONTENT edges, vector/attachment changes, incident-edge
and node deletes, and expected-revision guards all count toward the graph batch cap.
The A2 setup re-base removes the former closed exception list; there is no logical-only
mode or family-wide exemption.

## 9. Delivery, risks and open decisions

| Slice | Exit criteria |
|---|---|
| A1: crate-private core | Provisional graph binding, scope provisioning, entity/episodic remember, same-frame receipts, exact dedup, idempotency and budget/validation coverage; independently accepted and integrated, unreleased and not activated |
| A2: private candidate | Crate-private small forget and graph-TTL expiry with atomic failure, evidence, budget-boundary, receipt-replay and reopen tests; re-base the three oversized A2 golden setups into multi-step physical-cap writes; pending independent acceptance, unreleased and not activated. No facts, recall, resumable erasure, serving, public API or format change |
| B: recall planner | Filtered hybrid plus bounded temporal expansion, owner retention checks, rank/explain fixtures and existing vector-only exact/MMR integration; scan/work/cancellation coverage; no exhaustive-history claim |
| C: consolidation | Caller summaries, slot-guarded corrections, contradictions, stale-source reporting and resumable erasure barriers/fencing; crash/retry and maintenance tests pass |
| D: serving after approval | Explicit OpenAPI and wire approval before edits; REST/gRPC/MCP/CLI scope/authorization, limits, feed and unsupported-capability parity; no external provider enabled by default |

Main risks: scope leakage through graph expansion, misleading as-of completeness,
receipt/history growth, exclusive-slot races, shared-evidence erasure and unbounded
source/reranker work. The acceptance cases and barriers above are release requirements.

The owner accepted every recommendation below on 2026-11-02, including the serving approval for
slice D. Acceptance authorizes the work; each slice still meets its exit criteria above.

| Decision | Accepted approach |
|---|---|
| Public Rust names and error taxonomy | Review typed handles/errors before A; preserve underlying storage ambiguity and conflicts |
| Scope sharing and physical partitioning | Exact-scope allowlists, no fallback; caller-provisioned graph/collection bindings, not per-session graphs |
| Receipt retention and request canonicalization | Version the digest; require an explicit retry window and bounded receipt cleanup; no indefinite replay promise |
| Fact-slot schema ownership | Caller-declared exclusive/multivalued slots with stable revision guards; no inferred semantic uniqueness |
| Temporal dependency rollout | Enable facts/relations only after Gap 4 write/read/maintenance exits; fail explicitly rather than emulate validity |
| Recall weights and MMR composition | Relevance-only default, opt-in recency/importance; vector-only MMR until a separately tested combined objective is approved |
| Safety budget values and source deadlines | Adopt proposed caps provisionally; validate work accounting before B, adjust through review without performance claims |
| Summary invalidation and shared evidence | Retain sources by default; bounded stale/dependency reporting and conservative ownership checks before deletion |
| Erasure across backups/history | Return logical/pending outcomes; operator-owned purge and restore manifests; do not advertise secure erasure |
| Serving schema, feeds and adapters | Defer all to D approval, including temporal/TTL feed gaps and current REST/gRPC rerank-cap differences |
