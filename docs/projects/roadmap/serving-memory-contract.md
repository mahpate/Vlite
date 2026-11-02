# Serving contract: agent memory

Status: exact schema proposal for Gap 1 slice D; **not implemented or shipped**.
The owner authorized serving work on 2026-11-02, conditional on reviewing each
exact schema before code ([release decision](next-major.md#decided-2026-11-02)).
This document requests that review; it activates nothing. All names, bounds,
errors, fields and adapter methods below are proposed unless marked Current.
Public Rust memory names remain proposed pending slice A API review. Core-facing
shapes target the provisional, crate-private A1 candidate (Gap 1 section 2);
"Core alignment" in section 3 lists what may still change.

Authority: [Gap 1](gap1-agent-memory.md), read in full, especially sections 1-6
and 9. [Serving contract](serving-contract.md) supplies the Gap 8 provider and
Gap 12 graph-service dependencies. This document does not override either.
Scope: remember, recall, facts/correction, caller-produced consolidation, forget,
receipt reconciliation and resumable erasure across REST, gRPC, MCP, CLI and
Python/JavaScript/.NET HTTP clients. Non-goals: implementing the core, changing
formats/defaults/gates, automatic extraction, semantic deduplication, model
hosting, secure erasure, replication or benchmark/performance qualification.

## 1. Testable requirements

1. Enable a binding only after Gap 1 A-C, their Gap 2-4 prerequisites and relevant
   Gap 12 graph-service acceptance; reject unavailable capabilities explicitly.
2. Derive tenant from the authenticated principal, resolve the binding and
   authorize every exact scope before lookup, candidate discovery or provider work.
3. Commit every small mutation and its receipt in one graph frame; reject a
   whole bundle before append if any guard, authorization, count or byte check fails.
4. Replay a retained matching receipt without changing TTL, provenance or revisions;
   reject a changed canonical request with `idempotency_conflict`.
5. Apply owner retention and scope checks to every recall candidate. Capture one
   committed sequence, validity time T and TTL clock C, and expose diagnostics.
6. Preserve ranking meaning and deterministic ties for the same view/input;
   never describe bounded/approximate retrieval as exhaustive historical truth.
7. Distinguish logical deletion, pending cleanup and storage ambiguity on every
   adapter. Never translate partial results or cancellation into an empty success.
8. Apply the same memory limits/errors/authorization on all transports, without
   changing legacy search limits, graph grants, rate policy or provider defaults.
9. Keep OpenAPI/proto frozen until this document is approved; qualify each later
   slice independently against wire goldens and the independent logical models.

## 2. Current coverage and evidence

These are checkout observations, not memory implementations. Absence below is
limited to the cited dispatchers/types, not an assertion about future core work.

| Current surface | Verified evidence | Consequence for memory serving |
|---|---|---|
| One service owns one database handle | `src/server.rs:159-160`, `src/server.rs:207-224`; tenant policy and namespace usage: `src/server/tenants.rs:9-20`, `src/server/tenants.rs:51-69` | One process/file; tenants are policy within it, not separate databases |
| REST vector/search routes and graph routes | `src/server.rs:482-522`; `src/server/graph.rs:123-184` | Add memory routes; do not pretend existing graph endpoints are memory APIs |
| gRPC vector/search/import/feed service | `proto/vlite/v1/vlite.proto:16-60` | Add a separate Memory service; no existing memory RPC/receipt lookup there |
| MCP vector tool allowlist; graph CLI dispatch | `src/bin/vlite-mcp.rs:294-321`; `src/bin/vlite-graph-cli.rs:33-82` | New tools/commands need their own dispatch and strict validation |
| Route authorization, lineage and read-only rules | `src/server.rs:835-849`, `src/server.rs:967-1010`; graph authorization contract: `src/server/graph.rs:10-52` | Add explicit read-POST classification; unmapped routes are admin-only |
| Tenant admission and name qualification | `src/server.rs:803-815`; `src/server/tenants.rs:135-150`, `src/server/tenants.rs:462-478`; `src/server/credentials.rs:137-150` | No client tenant or physical-name escape; suspended/deleted tenants fail closed |
| Graph write plus record authorization/quota under guard | `src/server/graph.rs:705-784`; atomic entry point: `src/property_graph/mod.rs:397-403` | Reuse guarded planning, not separate vector/KV writes |
| Frame count/bytes and expiry accounting | `src/property_graph/engine/limits.rs:31-35`; `src/property_graph/mod.rs:592-628`, `src/property_graph/mod.rs:880-936` | 4,096 user operations, 16,777,216 encoded body bytes; reclaim bytes still count |
| Graph batch result is revision/outcomes, not a memory receipt | `src/property_graph/mod.rs:369-377`; imports have chunk retry semantics: `proto/vlite/v1/vlite.proto:49-58` | Neither is a general memory idempotency facility; propose graph receipts |
| REST rerank is vector-only, default max(k,50), cap 1,000 | `src/server.rs:2647-2671`, `src/server.rs:3032-3054` | Preserve legacy route behavior |
| gRPC rerank is vector-only, default 4*k, cap 4,096 | `src/server/grpc.rs:1081-1109`, `src/server/grpc.rs:1135-1153` | Do not silently unify old Search; new Memory.Recall uses section 4 |
| Exact distance/ID ties, cosine MMR and fetched-vector cap | `src/rerank.rs:21-51`, `src/rerank.rs:54-68`, `src/rerank.rs:90-104`, `src/rerank.rs:106-132` | Candidate-only repair/diversity, not historical recovery |
| REST errors and ambiguity | `src/server.rs:1399-1418`, `src/server.rs:1450-1586`, `src/server.rs:1587-1650` | Add memory codes in this server-private mapper; no public `Error` variant (A1 `MemoryError` is crate-private); old mappings unchanged |
| gRPC mapping and code/details trailers | `src/server/grpc.rs:181-215` | Today: 409 -> FAILED_PRECONDITION except `collection_exists` -> ALREADY_EXISTS and `rebuild_busy` -> ABORTED; 412 -> ABORTED; 503 `busy`/`unavailable` -> UNAVAILABLE, `commit_indeterminate` -> UNKNOWN, `poisoned` -> INTERNAL; other 4xx -> INVALID_ARGUMENT, other 5xx -> INTERNAL. Memory adds only the 413/499/504 rows of section 6 |
| REST body 8 MiB; gRPC request 8 MiB/response 16 MiB | `src/server.rs:60`, `src/server.rs:742`; `src/server/grpc.rs:22-25` | Wire caps are smaller than the atomic frame cap. Over-cap REST bodies are `invalid_json` with the extractor's 413 (`src/server.rs:1658-1662`); gRPC over-cap messages are refused by tonic as OUT_OF_RANGE before the adapter, with no `vlite-error-code` trailer (tonic 0.14.6 `src/codec/decode.rs:193-197`, `src/codec/encode.rs:188-192`; `Cargo.lock:2946-2947`) |
| Shared scoped-key token bucket and admission | `src/server.rs:3472-3506`; `src/server/grpc.rs:249-267` | No new default rate or unbounded memory queue |
| Feed visibility, temporal refusal, omitted expiry events | `src/server/graph.rs:85-108`; `src/property_graph/feed.rs:270-285` | No complete memory replication/TTL subscription promise |
| JSON graph events and webhook projection | `src/changes.rs:502-575`, `src/changes.rs:582-599`; `src/server/webhooks.rs:366-390` | Ordinary graph/record events only; see section 7 |
| Existing HTTP client search adapters | `clients/python/vlite_client/__init__.py:535-558`; `clients/javascript/index.js:229`; `clients/dotnet/VLite.Client/Client.cs:161-164` | Add typed memory facades; leave search return types unchanged |

## 3. Shared wire and identity rules

All subsequent sections are normative proposals. JSON objects reject duplicate
and unknown keys; required fields cannot be null. Optional fields are omitted,
except explicitly nullable diagnostics/bounds. Integers are not booleans. UTF-8
byte limits apply before allocation, including escaped JSON after decoding.
No Unicode normalization/case folding; reject control characters in opaque keys.
Identifiers (`Key`) are 1-256 UTF-8 bytes. `U64` is a canonical unsigned decimal
string, 0..18446744073709551615, no leading zero except "0". `I64` is a canonical
signed decimal string, -9223372036854775808..9223372036854775807, no "-0".
Times/revisions/sequences on JSON/MCP/CLI/SDK surfaces are strings, never lossy
JavaScript numbers. Protobuf uses the corresponding integer types.
Finite JSON numbers are binary64; vector inputs round once to finite binary32,
rejecting overflow and invalid dimensions against the provisioned collection.

Base path is `/v1/memory/{binding}`. `binding` is an unqualified local name matching
`^[a-z][a-z0-9_-]{0,62}$`; startup configuration maps `(lineage, principal tenant,
binding)` to exactly one provisioned memory graph, 0-16 evidence collections (A1
binds at most one), schema/ranking versions, exact scope allowlists and, from A2,
receipt retention.
No provisioning/configuration endpoint is added here. Setup is retryable and
separate from mutation atomicity (Gap 1 sections 1-2). Missing binding is
`memory_not_configured`, after authorization, with no existence probing.

Request `Scope` is `{agent:Key,user?:Key,session?:Key}`. Session requires user;
absence means shared in that exact tenant/agent, never wildcard/parent fallback.
Response `EffectiveScope` adds `tenant:Key`, derived only by the server. A
tenant-bound credential's `tenant` is its immutable `Tenant.id` (random 128-bit hex),
never the renamable name (`src/server/tenants.rs:241-245`, `:424`). Untenanted
admin/read-only/scoped principals select the proposed reserved `@root` sentinel,
which cannot collide with a hex id and appears nowhere in `src` today; a tenant
*named* `@root` is still allowed (`src/server/tenants.rs:47-52`). Neither is ever
a tenant named by a request: to operate as a tenant, use its tenant-bound credential.
No tenant field, tenant header, physical graph name or client-supplied collection
path is accepted. Each evidence `collection` is an alias in this binding.
Resolve aliases once to stored names; never double-qualify stored attachments.
Explicit startup scope grants bind principal identity to exact agent/user/session
tuples; the client may select only their intersection with its graph/collection
grants. A graph grant alone does not grant every user/session in a memory binding.
Unconfigured principals have no memory scopes, including local admin mode.

One mutation targets one exact scope. References, merges, corrections, evidence
owners, consolidation inputs and erasure targets must remain in it. Recall takes
1-16 distinct scopes (default: its single required scope); no implicit widening.
Canonical tuple framing/digest collision verification follows Gap 1 section 1.
Only response projections may redact optional sensitive fields; lack of access
to a requested scope or required evidence collection fails before execution.

### Atomicity, receipts and failures

Mutations require body `idempotency_key:Key`; no HTTP header alias is defined.
The identity is `(lineage,stable binding identity,exact scope,operation,key)`, where
operation is one of `remember,correct_facts,consolidate,forget,resume_erasure`.
The core computes the digest; an adapter never does. A1 uses `v1:` + SHA-256 of a
fixed-order canonical JSON text of scope, key, `receipt_window_ms`,
`allow_future_observations` and each item's content, kind, source, provenance,
references, importance, observation and merge fields, excluding `deadline_ms` and
the clock. `digest_version` `memory-request-v1` names that encoding; later versions
extend the enum additively. Fields A1 does not hash (vectors, metadata, evidence,
confidence, validity, policies, limits, erasure targets/tokens) join the digest only
in the core slice that accepts them, under a new version with byte goldens
published before adapter code; adapter fixtures come from core tests, never from
an oracle's digest.

Receipt creation-if-absent, object/slot revision guards and all changes share one
frame. Same retained identity/digest returns the original stable result and
sequence, even if targets later expire/disappear; set `replayed:true` only in the
response envelope. Different digest returns 409 `idempotency_conflict`, with no
mutation. A no-op still commits a receipt. Neither retry extends memory/receipt
TTL nor appends provenance. Receipt results contain IDs/outcomes, never forgotten
text, vectors, URIs or full provenance.
Receipts created by slice A1 never expire; expiry arrives with A2; serving slices start after A2 acceptance.
In A1 a replay (matching digest) is answered before the deadline check.

**A2-dependent retention design (not A1 behavior; changes with A2).** A binding
may set `receipt_window_ms` in 1..2592000000 with no implicit default; a receipt
then advertises `retry_until_ms` (checked captured-clock addition), at which
boundary lookup returns not found and the key may execute again, exact dedup still
applying. Cleanup is bounded, separate work, never an unbounded prefix scan.
Binding identity, schema, canonical defaults and receipt access stay stable for the
advertised window; incompatible binding replacement is rejected while retained
receipts/jobs need it. A1 instead requires a nonzero per-request `receipt_window_ms`,
hashed into the digest and otherwise ignored; section 8 has no such field, so
whether it survives, and what the adapter passes, is an owner decision ("Receipt
retention", section 11).

`Receipt` is `{id:Key,operation:Operation,scope:EffectiveScope,sequence:U64,
retry_until_ms?:U64,digest_version:"memory-request-v1"}`; `retry_until_ms` exists
only under the A2 design. `sequence` is A1's `commit_sequence` (Core alignment). A1
returns no receipt ID, so `id` needs a core accessor (owner decision, section 11).
Mutation success is `MutationResponse` from section 8. `outcomes` follow input
order; an exact dedup outcome returns the retained ID/revision. No per-item errors
or 207 status: any failed item aborts the whole small mutation including TTL cleanup.
Logical `created|merged|noop|corrected|deleted|expired|retained_shared` outcomes
are not partial commits. Duplicate bundle IDs and empty bundles reject explicitly.
Do not silently paraphrase-merge, overwrite anchors, raise importance or extend
retention; merging provenance requires `merge_provenance` and an exact revision.

On `commit_indeterminate`, stop writes on the poisoned handle, reopen a healthy
database, then lookup the receipt. Present means the whole frame committed;
absent allows retry, not proof that external systems did nothing. Preserve the
configured durability: sequence/atomic visibility is not unconditional power-loss
durability. No per-request durability switch; operators choose Full/sync policy
as in Gap 1 section 2. Never retry a mutation automatically on network timeout,
UNKNOWN or cancellation without receipt reconciliation.

### Core alignment (A1 candidate)

A1 is crate-private and provisional (Gap 1 section 2). If its contract changes,
this table and the section 8 examples change with it.

| Topic | A1 candidate behavior this contract relies on |
|---|---|
| Request digest | Core-computed `v1:` + SHA-256 of fixed-order JSON; `deadline_ms` and the clock are excluded |
| `content_digest` | `v1:` + 64 lowercase hex of SHA-256 over the JSON-quoted content string |
| Receipt | Graph node plus outcome chunk nodes in the mutation frame; never expires; no receipt ID is returned |
| Outcomes | `created`, `merged`, `noop` only; `deleted`, `expired`, `retained_shared` arrive with A2, `corrected` with A3 |
| `revision` / `sequence` | `revision` is the per-memory logical revision (1 at creation, +1 per merge, stored value on `noop`); `sequence` is the frame's commit sequence, equal to the receipt revision and unchanged on replay |
| Kinds | `entity`, `episodic`; `fact`, `relation` need A3 / Gap 4 |
| Deadline | The core's `deadline_ms` is absolute; the adapter computes it as `now_ms` plus the JSON `deadline_ms` budget (1..5,000 ms, default 250), checked once after replay and before planning, excluded from the digest |
| Provenance | At most 64 entries and 4,096 encoded bytes; each source, kind, URI and transformation field at most 256 bytes |
| Evidence | 0 or 1 collection; no vector attach |
| References | Bare item IDs, resolved in the bundle first, else in the same scope's retained state; no revision pin, so `Ref.revision` on item references is not enforceable yet (section 11) |
| Operations | 4,096 graph operations per frame, counting memory writes, receipt and outcome-chunk nodes, content-chunk nodes and HAS_CONTENT edges; a merge also charges one write plus one revision guard |
| Request | 1-4,096 items; opaque keys at most 256 UTF-8 bytes; canonical request and frame each at most 16 MiB |

## 4. Operations, fields, ranking and bounds

Every request/response field is defined by the schema inventory in section 8.
Defaults/conditional rules here are part of those schemas, not optional advice.
All methods are unary POSTs; there is no memory DELETE/GET/HEAD alias.

| REST suffix / operationId | Request -> response | Success | gRPC RPC | MCP tool | CLI verb |
|---|---|---|---|---|---|
| `/remember` / memoryRemember | RememberRequest -> MutationResponse | 200 | Remember | vlite_memory_remember | remember |
| `/recall` / memoryRecall | RecallRequest -> RecallResponse | 200 | Recall | vlite_memory_recall | recall |
| `/facts/query` / memoryFindFacts | FactsRequest -> FactsResponse | 200 | FindFacts | vlite_memory_facts | facts |
| `/facts/correct` / memoryCorrectFacts | CorrectionRequest -> MutationResponse | 200 | CorrectFacts | vlite_memory_correct_facts | correct-facts |
| `/consolidate` / memoryConsolidate | ConsolidateRequest -> MutationResponse or PlanResponse | 200 | Consolidate | vlite_memory_consolidate | consolidate |
| `/forget` / memoryForget | ForgetRequest -> MutationResponse | 200 logical / 202 pending | Forget | vlite_memory_forget | forget |
| `/receipts/lookup` / memoryGetReceipt | ReceiptRequest -> ReceiptResponse | 200 | GetReceipt | vlite_memory_receipt | receipt |
| `/erasures/get` / memoryGetErasure | ErasureRequest -> ErasureResponse | 200 | GetErasure | vlite_memory_erasure | erasure |
| `/erasures/resume` / memoryResumeErasure | ResumeRequest -> MutationResponse | 200 logical / 202 pending | ResumeErasure | vlite_memory_resume_erasure | resume-erasure |

**Remember.** `items` has 1-4,096 ordered memories, optional `evidence` 0-256.
Each item supplies its opaque `id`, kind, content, caller `observed_at_ms`,
source and references; `importance` defaults 0.5, optional confidence is separate.
Creation/update timestamps come from one engine clock and cannot be supplied.
Returned provenance adds `content_digest`, `v1:` plus 64 lowercase hex digits:
A1 hashes the JSON-quoted content string (quotes and escapes included), not the
bare UTF-8 bytes. Preserve original
digests when copying input provenance into a derived summary; do not relabel an
input's digest as the summary's digest. This is separate from the typed request
digest and scope/kind/source-qualified exact-dedup identity.
`expected` is `{absent:true}` for creation (default) or `{revision:U64}` for
explicit merge. `merge` is `none` (default) or `merge_provenance`.
`references` entries are `Ref`s: a retained memory's `{id,revision}` or, with
`revision` 0, an ID created in this bundle; resolve the whole bundle before append,
supporting forward references. Provenance entries describe sources, not memories.
Each item/evidence record belongs to the single request scope.
Evidence IDs are caller-supplied U64, new versioned records only, expected absent;
vectors are optional, not fabricated zeros; text-only records are supported.
Source URI is inert, never fetched/executed. Item content is exact-dedup material;
digest plus kind/scope/source identity determines dedup, not vector similarity.
`fact|relation` requires `assertion`, forbids it otherwise; subject/destination
must be entity anchors. Slot identity is subject/predicate/slot_key in this scope;
exclusive/multivalued mode is predeclared in binding schema, not request-controlled.
Require `slot_expected` and all changed existing revisions. `disagreement` defaults
`reject`; explicit `disputed` stores labelled disagreement without declaring truth.
The interval is half-open; null endpoints mean unbounded, from < to when both
finite. There are at most 256 retained segments per series (Gap 1 sections 1-2).

**Recall.** At least one of vector, text or entity_seeds is required. Text is
lexical by default; no implicit embedding. `embedding_provider` requires text,
forbids an explicit vector and selects only a Gap 8 authorized registry alias.
Providers are off by default, never inferred from environment credentials;
no endpoint/model/secret may appear in a memory request. Reauthorize and recheck
revisions/retention after provider work as required by the serving contract.
No network operation is inside a memory write frame; remember/consolidate require
caller-produced embeddings. Custom/LLM/cross-encoder rerankers are not exposed
in this version; unsupported selection is refused, not silently substituted.

`mode` defaults `exact`, alternative `approximate` requires ready indexes; no lazy
index build. Candidate union default 200, ceiling 1,000 **on all memory adapters**,
also the exact/MMR candidate cap; `limit` default 20, ceiling 100, <= candidates.
This intentionally does not alter the legacy rerank candidate caps (REST 1,000, gRPC 4,096) in section 2. Charge duplicate examinations across legs/seeds, not just unique
output IDs. Exact preflight refuses a source >100,000 records. Candidate limits
do not bound index construction/filter/BM25 work; unsupported bounded plans fail.
Conjoin typed caller filters with mandatory scope/kind/retention checks before
discovery and return; OR cannot replace those predicates. Expand through in-scope,
TTL-live owners/edges and valid accepted/disputed segments; revalidate evidence.
As-of T (`as_of_ms`) defaults to the checked engine-clock reading; C (`ttl_clock_ms`)
is a separate engine-clock reading, never supplied and never derived from T.
At C == expiry hide owner and evidence, regardless of T.

Rank eligible fused candidates using `R=1/rank`, `age=max(0,T-observed_at_ms)`,
`D=1/(1+age/half_life_ms)`, `I=importance`, `S=wR*R+wD*D+wI*I`.
Policy version `memory-ranking-v1` defaults weights `[1,0,0]`; weights finite,
nonnegative with abs(sum-1) <= 1e-12, no renormalization. Positive finite
half_life_ms is required when wD>0; absent means D=null when wD=0.
Future observations default `reject`, explicit `clamp` permits age=0;
check signed subtraction overflow before clamping. Expanded-only R=0.
`selection` defaults `weighted`; `exact` recomputes vector-candidate distances,
reorders their dense leg before fusion/weighting, preserving text-only candidates.
`mmr` is a separate vector-only cosine selection, lambda default 0.5 in [0,1]:
reject text/hybrid/entity expansion, non-default weights, decay or nonvector
candidates with `memory_unsupported_ranking`. MMR returns selection order, retains
cosine distance, and S remains a diagnostic relevance value, not its objective.
No fused/BM25 value is reported as cosine similarity. Exact/MMR cannot recover
omitted candidates. Vector ties use record ID as the existing reranker does;
cross-collection/scoped ties extend that order by collection alias and memory key.
Weighted final ordering is descending S, canonical scope (tenant,agent,user,session;
null before present, UTF-8 lexical), memory ID, then segment bounds (null segment
first; lower unbounded before finite, upper unbounded after finite).
Provenance is sorted by source kind/ID/URI then memory ID/revision; paths by
scope, node-ID sequence, then edge-ID sequence; evidence by alias/record ID.
No bitwise/platform/ANN-set score stability promise: stability is policy-versioned
for fixed eligible input/view; SDKs must not compare scores across versions.

`projection` defaults snippets=true, provenance=true, paths=false, metadata=[];
snippets <=4,096 UTF-8 bytes each, total evidence budget charged before fetch.
`RecallResponse` includes matched segments, contradiction/staleness labels,
raw dense/sparse/text ranks and scores, distance/BM25, R/D/I/S, mode/policy,
effective scopes/T/C/sequence, examined/charged work and completeness reasons.
`complete` means the declared bounded plan completed, not complete semantic or
historical knowledge. Every successful read lists `retained_state_only`; an
explicit `as_of_ms` also lists
`historical_candidate_discovery_not_exhaustive` and does not reconstruct deleted
records, old anchor metadata or an old index. These limitations alone are not budget
truncation. There is **no recall pagination** or cursor in this version.
Budget exhaustion returns a partial 200 only with `allow_partial:true`, with
complete=false and explicit truncation; otherwise a typed budget/deadline error.
Authorization/storage/cancellation errors never return partial data.

**Facts and corrections.** Query requires one exact subject/predicate/slot_key;
default `history:false` selects at T, history=true returns retained segments and
forbids as_of_ms. Limit 20/100, no paging; exceeding output/work budgets follows
allow_partial, never silently truncates. Order by series ID, segment lower/upper
bounds, destination ID; include evidence versions and accepted/disputed/stale state.
Correction supplies `close|replace|remove`, exact series and slot revisions.
Close/replace requires at_ms; replacement requires a complete new fact/relation
item and evidence; remove forbids both at_ms and replacement and deletes the
selected segment (not privacy erasure). The required `interval` identifies the
existing segment. Explicit interval correction supplies `correction_interval`
in place of at_ms; close forbids interval correction in this version.
Replace closes/replaces evidence in one frame; replacement keeps the exact scope
and slot, may select a new destination/series. Honor Gap 4 half-open bounds and
exact-end no-op (Gap 1 section 4), including no overlapping exclusive destinations.
No arbitrary graph text write or automatic conflict resolution is exposed.

**Consolidate.** Caller supplies one episodic output item with transformation_id,
1-256 exact input ID/revision/disposition entries and optional evidence.
Disposition defaults retain; explicit logical_delete obeys forget ownership checks.
Do not confuse consolidation with forgetting. Dry_run defaults false; true returns
PlanResponse conflicts/counts/bytes, no receipt/write/reservation. Commit rechecks
the whole retained input set and slot/ownership guards. Output provenance links
every input; duplicates reject. Bounded stale-summary invalidation reports
`remaining_work` (count or null), not a completed cascade claim.

### Limits shared by all memory operations

| Limit | Default / hard maximum and exact enforcement | Basis |
|---|---|---|
| Request, serialized success/error response | 8,388,608 bytes each, REST/MCP/CLI/SDK; gRPC request 8,388,608, response 16,777,216; projected memory JSON equivalent also <=8,388,608 | Request caps Current (`src/server.rs:60,742`; `src/server/grpc.rs:22-25`); response caps and projected equivalent New |
| Mutation input entries / planned user operations / frame body | 4,096 input items or explicit targets / 4,096 operations (counted as in Core alignment) / 16,777,216 encoded frame bytes, including receipts, temporal writes and reclaim bytes; never auto-split | Frame caps Current (`src/property_graph/engine/limits.rs:33-35`); counting A1 |
| Evidence | 256 input records/bundle, 64 links/item; payload <=1,048,576 decoded bytes/record; total <=4,194,304 bytes, summing UTF-8 text bytes + decoded payload bytes + 4*vector components + encoded metadata bytes | New; total from Gap 1 section 6 |
| Content / query text / source URI | 4,096 / 16,384 / 256 UTF-8 bytes; source_id/source_kind/transformation_id are Key | URI A1; content cap is stricter than A1's chunked content; query text New |
| Metadata | 64 typed scalar fields, 65,536 encoded property bytes, strings <=4,096 bytes; whole reserved+caller object must fit, no overwrite of reserved fields; the core slice that first accepts caller metadata publishes the reserved names (A1 accepts none) and collisions reject `invalid_input` | Current (`limits.rs:24-30`) |
| References/provenance/input cap | max_references 64/256 caps each item's reference count and a consolidation's input count; provenance also <=64 entries and 4,096 encoded bytes; reject overflow, never drop provenance | Gap 1 section 6; provenance A1 |
| Recall scopes / output / candidates | 1/16, 20/100, 200/1,000; entity seeds <=64; distinct kinds <=4; filter conditions <=64 | Gap 1 section 6; seeds and kinds New; conditions Current (`limits.rs:37`) |
| Exact scan | 100,000 records across all bound sources; reject preflight, no automatic approximate fallback | Gap 1 section 6 |
| Vector components fetched | 1,000,000/4,000,000; checked count*dimension, charge before fetch | Gap 1 section 6 |
| Evidence bytes fetched and output | 1,048,576/4,194,304 each; truncation requires allow_partial and explicit reason | Gap 1 section 6 |
| MMR component work | 10,000,000/100,000,000; pair comparisons*dimensions, checked before work | Gap 1 section 6 |
| Series segments | 256, corrections counted as user operations | Gap 1 section 6 |
| Graph hops / visited / edges / per-node edges | 2/4, 1,000/10,000, 10,000/100,000, 1,000/10,000 | Current (`limits.rs:87-108`) |
| Graph output nodes / edges / paths | 100/1,000, 200/2,000, 20/100 | Current (`limits.rs:87-108`) |
| Deadline | 250/5,000 ms total monotonic elapsed including admission/provider work; client deadline wins; never silently extend | Graph reads Current (`limits.rs:93,104`); mutation default New, A1 has none (section 11) |

Graph defaults/ceilings are Current at
`src/property_graph/engine/limits.rs:87-108`; zero hops/outputs are legal, zero
work/deadline budgets and above-ceiling values reject rather than clamp
(`src/property_graph/engine/limits.rs:65-69`, `src/property_graph/engine/limits.rs:125-157`).
Other memory limits are proposed Gap 1 section 6 safety budgets and explicit
wire constraints, not size/performance measurements. Read caps do not excuse
unaccounted planner work. Reject an unrepresentable response before mutation
append; reserve bounded receipt/outcome response space during planning.
`max_references` (1..256, default 64) caps each item's reference count and a
consolidation's input count separately; provenance is capped by the A1 core, not
by `max_references`.

## 5. Forget, jobs and wire caveats

Forget target is exactly one of IDs+revisions, entire fact series+revision,
entity+revision, session (request scope must contain it), or exact scope.
`mode` is required: `logical_delete` or `expire`; expire requires expires_at_ms,
logical_delete forbids it. Validity close is not an alias for either.
Missing explicit targets return `memory_not_found`; retained receipt replay is
the way to make retries safe. Entity deletion includes bounded incident edges.
Small logical delete detaches authorized evidence, deletes only unshared records,
deletes whole selected series/history and unneeded anchors in one receipt frame.
External attachments block, never detach them opportunistically. Shared evidence
is retained and reported. Expiry hides graph owners; it is not record erasure.

Oversized entity/session/scope deletion requires `allow_job:true` (default false);
otherwise 422 `invalid_input`, no barrier. Expire is atomic-only in this
version. A job first commits its scope/entity barrier and receipt. Thereafter
every memory read/write, including broad recall and receipt-independent retries,
checks the barrier; reads omit hidden items with `deletion_barrier` diagnostics,
writes to it fail `memory_deletion_pending`. Receipt/job lookup remains available.
Scope/session jobs cover the exact requested scope/session, not child scopes.
No automatic scope reactivation. Large ID/series lists must be explicitly split;
do not create a surprise non-atomic job for them.

`Erasure` is `{id,scope,target,revision,generation,state,logical_visibility,
deleted_count,remaining_count,resume_token}`; state pending|logical, visibility
hidden, remaining_count is U64 or null (unknown), token is opaque <=4,096 bytes
and absent when finished. Get returns status only. Resume supplies the exact job
revision/generation/token and a new idempotency key; one bounded chunk and its
progress/manifest/receipt commit together. Token binds lineage, binding, scope,
target, generation, revision and next chunk identity; stale token/worker conflicts.
Replaying the same resume key returns the original chunk result; concurrent next
chunks cannot both commit. Cap chunks at the same 4,096 operations/body limit.
Operator reconciliation is required after restore/revision-changing maintenance;
reject `memory_job_reconciliation_required`, never guess a continuation.

Every Forget/Resume response and their receipt replay/GetErasure response carries:

```json
{"erasure_notice":{"live_state_only":true,"secure_erasure":false,
 "external_copies":"operator_managed","restore_requires_manifest":true,
 "backup_history_purge":"not_verified","manifest_id":"opaque-id"}}
```

Manifest ID is present for logical deletion/jobs, absent for retention expiry.
Manifests retain target identities/policy, never content or sensitive source URIs;
their retention is an explicit operator policy, separate from receipt TTL.
Deletion outcomes `logical` (HTTP 200) and `pending` (HTTP 202) never mean secure
overwrite, backup purge or consumer acknowledgement. Retention expiry instead
returns state `retention_set` (HTTP 200), including an `expired` outcome and the
committed deadline; a future deadline does not claim that content is already hidden.
Append logs/checkpoints/exports,
backups/replicas/feeds/caches and retained summaries can retain old content;
compaction is not secure erase and encryption is not per-memory cryptographic
erasure. Restore can resurrect content unless deletion manifests are reapplied.
Caller-owned consumers acknowledge their purge separately (Gap 1 section 5).

## 6. Authorization, errors and observability

No new credential operation is introduced. Every operation requires binding and
exact-scope grants, correct lineage, active tenant and authorization under the
same view/write guard used for planning. A `collections:["*"]` grant never confers
graph or memory scope (`src/server/graph.rs:22-24`); memory access needs
`graph_read`/`graph_write` plus an exact startup scope grant.
Admin authority does not synthesize tenant/scope from request data; read-only keys
may use the four read operations only. Configure explicit root scopes if needed.
MCP/CLI below are authenticated remote adapters, not direct-file scope bypasses.

| Methods | Required existing operations and binding checks |
|---|---|
| Recall | graph_read + read + search for every evidence collection; provider alias additionally authorized by Gap 8 |
| FindFacts | graph_read + read for all evidence collections the response can expose |
| GetReceipt, GetErasure | graph_read, exact scope and binding; receipts contain no evidence content |
| Remember, CorrectFacts, Consolidate, Forget, ResumeErasure | graph_write + write for every potentially touched evidence collection, including detach/delete/attached-node effects; graph_read for referenced existing memories |

Mutating routes require write grants even when dry_run or a potential no-op.
Do not infer authorization from a caller's revision/receipt/job token. Recheck
queued credentials, tenant suspension/revocation, binding generation and scope.
Preserve vector quotas and reject atomically. Unauthorized targets fail 403
before existence lookup; authorized missing memory/receipt/job is 404.

A memory binding reserves its graph and evidence collections. Its explicit
startup policy additionally has `raw_access:false` per principal by default:
memory operation grants do **not** authorize raw graph/vector/search/export/feed
access to those resources. Apply that check at all raw server adapters and
attachment/retrieval paths before lookup; filter reserved resources from lists
and feeds while advancing cursors normally. Raw writes must also be blocked:
otherwise a caller could alter scope/owner/receipt/barrier metadata directly.
Only explicitly broad operator principals with raw_access=true plus the normal
underlying grants can use those raw surfaces. Never use raw access as a memory
scope grant. This is part of explicit new-binding provisioning/approval, not a
retroactive policy change for ordinary graphs/collections; refuse to adopt
in-use ordinary resources without an independently approved migration. Embedded
file access and operator backups remain outside server scope enforcement.

REST errors retain `{error:{code,message}}`, with optional new `details` only on
these routes: `{field?,limit?,requested?,competing?:[{id,interval,revision}],
receipt_lookup_required?:boolean,code?:string}` (`code` only on `PlanResponse.conflicts`
items, using a code below); no text/vectors/URIs/secrets. gRPC carries the
same code in `vlite-error-code`, details JSON in `vlite-error-details-base64`,
request ID in `x-request-id`. The following table is exhaustive for new memory
domain errors; unchanged underlying auth/quota/provider/graph codes remain as
specified in their existing mapper or the approved serving contract.

| REST / code | gRPC | Meaning |
|---|---|---|
| 400 invalid_json | INVALID_ARGUMENT | Malformed JSON/duplicates; the status is the extractor's, 400/413/415/422 (`src/server.rs:1658-1662`) |
| 401 unauthorized; 403 forbidden_scope/read_only/tenant_suspended/tenant_unavailable | UNAUTHENTICATED / PERMISSION_DENIED | No lookup or egress |
| 404 memory_not_found/receipt_not_found/erasure_not_found | NOT_FOUND | Authorized absent target (receipt expiry only under A2) |
| 409 memory_not_configured/memory_capability_unavailable | FAILED_PRECONDITION | Binding/core/temporal activation missing |
| 409 memory_feed_unsupported | FAILED_PRECONDITION | Requested complete temporal/TTL feed unavailable |
| 409 idempotency_conflict/memory_slot_conflict/memory_attachment_conflict | FAILED_PRECONDITION | Digest/exclusive slot/external ownership conflict |
| 409 memory_deletion_pending/memory_job_reconciliation_required | FAILED_PRECONDITION | Barrier or maintenance reconciliation |
| 409 index_not_ready | FAILED_PRECONDITION | No lazy read build |
| 412 precondition_failed | ABORTED | Existing code (`src/server.rs:1523-1525`): object/slot/input/job revision or generation mismatch; `details.field` says which |
| 413 memory_request_too_large/memory_response_too_large | RESOURCE_EXHAUSTED | Wire size refusal, before mutation append |
| 422 invalid_input | INVALID_ARGUMENT | Existing semantic code: shape, arithmetic, batch/frame bounds; `details` names the limit |
| 422 memory_unsupported_ranking/memory_bounded_plan_unsupported | INVALID_ARGUMENT | No semantic fallback |
| 429 memory_budget_exceeded/rate_limited | RESOURCE_EXHAUSTED | Explicit budget failure/shared credential limiter |
| 503 busy/unavailable | UNAVAILABLE | Existing admission saturation / service unavailable |
| 503 poisoned | INTERNAL | Existing code (`src/server.rs:1589-1591`); today's gRPC fallthrough (`src/server/grpc.rs:181-195`) |
| 503 commit_indeterminate | UNKNOWN | Reopen and reconcile receipt, not blind retry |
| 504 memory_deadline_exceeded | DEADLINE_EXCEEDED | No permitted partial answer |
| 499 memory_cancelled | CANCELLED | Cancellation before result; mutation may need receipt lookup |
| 500 internal_error | INTERNAL | Sanitized unexpected failure or integrity fault (A1 `DigestCollision`), never an empty result |

Add explicit gRPC mapping for the new 413/499/504 codes; `poisoned` keeps today's
INTERNAL mapping, and the generic 4xx/5xx mapping must not supply the wrong status
for the new codes. Today's REST over-cap `invalid_json` 413 and gRPC OUT_OF_RANGE
(section 2) make the gRPC halves of the 413 rows unreachable; see "Oversize
behaviour" in section 11. 429 responses
retain Retry-After:1/retry-after:1; budget details explain which budget rather than
suggest that time alone increases a hard cap. No unbounded retries/queues.
Use the existing per-credential configured token bucket, one charge per call
including replay/status, and shared admission. No new requests/second default
or exemption; an admin without a configured rate is not assigned one here.

Extend shared observation with event `memory_request`, route template, operation,
request_id, sanitized principal/tenant attribution, binding alias, status/code,
duration, input/output counts, candidate/edge/component/evidence-byte counters,
partial reasons, replay boolean and logical/pending state. Mutation audit records
receipt/job opaque ID and committed sequence, not memory IDs/content/key digests.
Metrics labels are operation/status/code only, never raw IDs, tenant/user/session,
idempotency keys or text. Do not log request/response bodies, embeddings, filters,
provenance, snippets, source URIs, tokens or provider responses.
Current shared observation already sanitizes values and audits scoped denials
(`src/server.rs:1203-1249`); memory success/audit fields are proposed additions,
not a claim that existing metrics implement this inventory.

## 7. Changes and webhooks

No new memory event, Memory.Watch RPC or webhook subscription API is proposed.
Committed memory nodes/receipts/jobs/manifests and evidence writes use ordinary
authorized graph_batch/record events where those events are representable;
preserve sequence+ordinal/frame grouping and existing cursor semantics.
Receipt replay emits no event. Barriers and progress are logical graph changes,
not proof that evidence bytes or backups have been erased. Do not expose a feed
as a scope-filtered memory API: a raw graph feed requires its graph grant and
explicit raw_access and may contain data from other scopes sharing that graph.
Memory-only principals cannot subscribe by reusing their underlying graph grant.
Operators must not grant raw access to principals requiring narrowed projections.

Temporal entries currently refuse in served feeds; deadline/reclaim events are
omitted (section 2). New response `feed` is always
`{mode:"ordinary_changes",temporal_complete:false,ttl_complete:false,
consumer_purge_acknowledged:false}`. `require_complete_feed` defaults false on
mutations, true returns `memory_feed_unsupported` **before** writes/providers;
there is no success-shaped placeholder temporal event or silent expiry feed.
Temporal read/write capability itself is gated independently; unsupported
as_of/fact/TTL requests return `memory_capability_unavailable`, not metadata
emulation. Rejecting complete feeds does not promise that ordinary consumers
can tail a temporal file: they may encounter the existing explicit refusal.
Do not modify existing feeds/webhooks to swallow that error. Any future additive
temporal/TTL events require separate exact schema/consumer approval in Gap 4/12;
this document reserves no event names or proto enum values for them.

## 8. Exact schema and adapter inventory

`src/openapi.json` and `proto/vlite/v1/vlite.proto` stay frozen until the owner
approves this document. The following is the proposed additive diff, not edits
to either file. Use tag `Memory`, bearer security, required path binding and
application/json bodies. Every route has the error/status rows in section 6;
only Forget/Resume additionally return 202. Existing paths/components/tags are
untouched. Field limits/conditional validation in sections 3-5 are normative
even where OpenAPI cannot express cross-field byte/work constraints.

### JSON exchanges

These are proposed wire goldens, not executed results. Each line is an
independently seeded exchange under binding `notes`, principal tenant `@root`,
scope `{agent:"a"}` and engine clock 100.
The wrapper fields `operation`, `request`, `status`, `response` are fixture
notation only; request and response are the actual JSON bodies.

```jsonl
{"operation":"remember","request":{"scope":{"agent":"a"},"idempotency_key":"k1","items":[{"id":"e1","kind":"entity","content":"Ada","observed_at_ms":"90","source":{"source_id":"s1","source_kind":"caller"}}]},"status":200,"response":{"receipt":{"id":"r1","operation":"remember","scope":{"tenant":"@root","agent":"a"},"sequence":"7","digest_version":"memory-request-v1"},"replayed":false,"outcomes":[{"id":"e1","action":"created","revision":"1"}],"feed":{"mode":"ordinary_changes","temporal_complete":false,"ttl_complete":false,"consumer_purge_acknowledged":false},"remaining_work":"0"}}
{"operation":"recall","request":{"scope":{"agent":"a"},"entity_seeds":["e1"],"limit":1},"status":200,"response":{"memories":[{"id":"e1","revision":"1","scope":{"tenant":"@root","agent":"a"},"kind":"entity","content":"Ada","observed_at_ms":"90","created_at_ms":"100","updated_at_ms":"100","importance":0.5,"status":"accepted","stale":false,"segment":null,"evidence":[],"scores":{"R":0,"D":null,"I":0.5,"S":0},"provenance":[{"source_id":"s1","source_kind":"caller","content_digest":"v1:a39afeed7d3319213be7a235840b3c6d3f09f2810b9c91779f34572b6b36832a"}],"references":[],"metadata":{}}],"meta":{"scopes":[{"tenant":"@root","agent":"a"}],"as_of_ms":"100","ttl_clock_ms":"100","sequence":"7","complete":true,"truncation":[],"limitations":["retained_state_only"],"mode":"bounded_expansion","candidates_examined":"1","edges_charged":"0","vector_components":"0","mmr_components":"0","evidence_bytes":"0"},"ranking_version":"memory-ranking-v1","selection":"weighted"}}
{"operation":"facts","request":{"scope":{"agent":"a"},"slot":{"subject_id":"e1","predicate":"role","slot_key":"primary"}},"status":200,"response":{"facts":[],"meta":{"scopes":[{"tenant":"@root","agent":"a"}],"as_of_ms":"100","ttl_clock_ms":"100","sequence":"7","complete":true,"truncation":[],"limitations":["retained_state_only"],"mode":"exact_enumeration","candidates_examined":"0","edges_charged":"0","vector_components":"0","mmr_components":"0","evidence_bytes":"0"}}}
{"operation":"correct_facts","request":{"scope":{"agent":"a"},"idempotency_key":"k1","slot":{"subject_id":"e1","predicate":"role","slot_key":"primary"},"series":{"id":"f1","revision":"7"},"slot_revision":"7","interval":{"from_ms":"0","to_ms":null},"action":"close","at_ms":"100"},"status":200,"response":{"receipt":{"id":"r1","operation":"correct_facts","scope":{"tenant":"@root","agent":"a"},"sequence":"8","digest_version":"memory-request-v1"},"replayed":false,"outcomes":[{"id":"f1","action":"corrected","revision":"8"}],"feed":{"mode":"ordinary_changes","temporal_complete":false,"ttl_complete":false,"consumer_purge_acknowledged":false},"remaining_work":"0"}}
{"operation":"consolidate","request":{"scope":{"agent":"a"},"idempotency_key":"k1","inputs":[{"id":"m1","revision":"7"}],"output":{"id":"summary1","kind":"episodic","content":"Caller summary","observed_at_ms":"90","source":{"source_id":"summary-source","source_kind":"caller","transformation_id":"summary-v1"}},"dry_run":true},"status":200,"response":{"dry_run":true,"user_operations":"4","encoded_bytes":"512","conflicts":[],"remaining_work":"0"}}
{"operation":"forget","request":{"scope":{"agent":"a"},"idempotency_key":"k1","target":{"ids":[{"id":"m1","revision":"7"}]},"mode":"logical_delete"},"status":200,"response":{"receipt":{"id":"r1","operation":"forget","scope":{"tenant":"@root","agent":"a"},"sequence":"8","digest_version":"memory-request-v1"},"replayed":false,"outcomes":[{"id":"m1","action":"deleted","revision":"8"}],"feed":{"mode":"ordinary_changes","temporal_complete":false,"ttl_complete":false,"consumer_purge_acknowledged":false},"remaining_work":"0","state":"logical","erasure_notice":{"live_state_only":true,"secure_erasure":false,"external_copies":"operator_managed","restore_requires_manifest":true,"backup_history_purge":"not_verified","manifest_id":"manifest1"}}}
{"operation":"receipt","request":{"scope":{"agent":"a"},"operation":"forget","idempotency_key":"k1"},"status":200,"response":{"result":{"receipt":{"id":"r1","operation":"forget","scope":{"tenant":"@root","agent":"a"},"sequence":"8","digest_version":"memory-request-v1"},"replayed":true,"outcomes":[{"id":"m1","action":"deleted","revision":"8"}],"feed":{"mode":"ordinary_changes","temporal_complete":false,"ttl_complete":false,"consumer_purge_acknowledged":false},"remaining_work":"0","state":"logical","erasure_notice":{"live_state_only":true,"secure_erasure":false,"external_copies":"operator_managed","restore_requires_manifest":true,"backup_history_purge":"not_verified","manifest_id":"manifest1"}}}}
{"operation":"erasure","request":{"scope":{"agent":"a"},"job_id":"j1"},"status":200,"response":{"erasure":{"id":"j1","scope":{"tenant":"@root","agent":"a"},"target":{"scope":true},"revision":"8","generation":"1","state":"pending","logical_visibility":"hidden","deleted_count":"0","remaining_count":null,"resume_token":"opaque1"},"erasure_notice":{"live_state_only":true,"secure_erasure":false,"external_copies":"operator_managed","restore_requires_manifest":true,"backup_history_purge":"not_verified","manifest_id":"manifest1"},"feed":{"mode":"ordinary_changes","temporal_complete":false,"ttl_complete":false,"consumer_purge_acknowledged":false}}}
{"operation":"resume_erasure","request":{"scope":{"agent":"a"},"idempotency_key":"chunk1","job_id":"j1","revision":"8","generation":"1","resume_token":"opaque1"},"status":202,"response":{"receipt":{"id":"r2","operation":"resume_erasure","scope":{"tenant":"@root","agent":"a"},"sequence":"9","digest_version":"memory-request-v1"},"replayed":false,"outcomes":[],"feed":{"mode":"ordinary_changes","temporal_complete":false,"ttl_complete":false,"consumer_purge_acknowledged":false},"remaining_work":null,"state":"pending","erasure":{"id":"j1","scope":{"tenant":"@root","agent":"a"},"target":{"scope":true},"revision":"9","generation":"1","state":"pending","logical_visibility":"hidden","deleted_count":"1","remaining_count":null,"resume_token":"opaque2"},"erasure_notice":{"live_state_only":true,"secure_erasure":false,"external_copies":"operator_managed","restore_requires_manifest":true,"backup_history_purge":"not_verified","manifest_id":"manifest1"}}}
```

The dry-run numbers are an injected planner fixture to illustrate integer
encoding, not measured/estimated frame sizes or a claim about a core encoding.
The implementation must supply independently verified count/byte fixtures.

### OpenAPI paths and components

This compact YAML uses an explicitly defined expansion: `x-memory-post` entries
expand to POST operations with operationId, tags:[Memory], security:[{BearerAuth:[]}],
one required binding path parameter using Binding, one required JSON requestBody
using the named request schema, 200 JSON response using the named result schema
(oneOf for a result list), and all section 6 errors using MemoryError.
`pending:true` adds 202 with the same result schema. These extension keys are
design notation only, not keys to ship in OpenAPI. No endpoint is implicit.
Reuse the Current `BearerAuth` scheme (`src/openapi.json:122-123`), not a newly
named security scheme.
All object schemas below have additionalProperties:false, except declared maps.
References use local component names (expand `ref: X` to `$ref:
'#/components/schemas/X'`); `array: X` means type:array/items:$ref X.
This finite expansion defines exact components without copying identical
operation/error boilerplate nine times. String limits below count code points
only as a preliminary schema check; normative UTF-8 byte checks still apply.

```yaml
tags: [{name: Memory, description: Explicitly provisioned scoped agent memory}]
paths:
  /v1/memory/{binding}/remember: {x-memory-post: {id: memoryRemember, request: RememberRequest, result: MutationResponse}}
  /v1/memory/{binding}/recall: {x-memory-post: {id: memoryRecall, request: RecallRequest, result: RecallResponse}}
  /v1/memory/{binding}/facts/query: {x-memory-post: {id: memoryFindFacts, request: FactsRequest, result: FactsResponse}}
  /v1/memory/{binding}/facts/correct: {x-memory-post: {id: memoryCorrectFacts, request: CorrectionRequest, result: MutationResponse}}
  /v1/memory/{binding}/consolidate: {x-memory-post: {id: memoryConsolidate, request: ConsolidateRequest, result: [MutationResponse, PlanResponse]}}
  /v1/memory/{binding}/forget: {x-memory-post: {id: memoryForget, request: ForgetRequest, result: MutationResponse, pending: true}}
  /v1/memory/{binding}/receipts/lookup: {x-memory-post: {id: memoryGetReceipt, request: ReceiptRequest, result: ReceiptResponse}}
  /v1/memory/{binding}/erasures/get: {x-memory-post: {id: memoryGetErasure, request: ErasureRequest, result: ErasureResponse}}
  /v1/memory/{binding}/erasures/resume: {x-memory-post: {id: memoryResumeErasure, request: ResumeRequest, result: MutationResponse, pending: true}}
components:
  schemas:
    Binding: {type: string, pattern: '^[a-z][a-z0-9_-]{0,62}$'}
    Key: {type: string, minLength: 1, maxLength: 256}
    U64: {type: string, pattern: '^(0|[1-9][0-9]*)$'}
    I64: {type: string, pattern: '^(0|-?[1-9][0-9]*)$'}
    Kind: {type: string, enum: [entity, episodic, fact, relation]}
    Operation: {type: string, enum: [remember, correct_facts, consolidate, forget, resume_erasure]}
    Scope: {type: object, required: [agent], properties: {agent: {ref: Key}, user: {ref: Key}, session: {ref: Key}}}
    EffectiveScope: {type: object, required: [tenant, agent], properties: {tenant: {ref: Key}, agent: {ref: Key}, user: {ref: Key}, session: {ref: Key}}}
    Expected: {oneOf: [{type: object, required: [absent], properties: {absent: {type: boolean, enum: [true]}}}, {type: object, required: [revision], properties: {revision: {ref: U64}}}]}
    Interval: {type: object, required: [from_ms, to_ms], properties: {from_ms: {ref: I64, nullable: true}, to_ms: {ref: I64, nullable: true}}}
    Ref: {type: object, required: [id, revision], properties: {id: {ref: Key}, revision: {ref: U64}}}
    Source: {type: object, required: [source_id, source_kind], properties: {source_id: {ref: Key}, source_kind: {ref: Key}, uri: {type: string, maxLength: 256}, transformation_id: {ref: Key}}}
    Provenance: {type: object, required: [source_id, source_kind, content_digest], properties: {source_id: {ref: Key}, source_kind: {ref: Key}, uri: {type: string, maxLength: 256}, transformation_id: {ref: Key}, content_digest: {type: string, pattern: '^v1:[0-9a-f]{64}$'}}}
    Scalar: {oneOf: [{type: object, required: [type, value], properties: {type: {enum: [string]}, value: {type: string, maxLength: 4096}}}, {type: object, required: [type, value], properties: {type: {enum: [integer]}, value: {ref: I64}}}, {type: object, required: [type, value], properties: {type: {enum: [float]}, value: {type: number}}}, {type: object, required: [type, value], properties: {type: {enum: [boolean]}, value: {type: boolean}}}]}
    Properties: {type: object, maxProperties: 64, additionalProperties: {ref: Scalar}}
    MemoryCondition: {type: object, required: [field, op], properties: {field: {ref: Key}, op: {enum: [eq, lt, lte, gt, gte, exists]}, value: {ref: Scalar}}}
    Filter: {type: object, properties: {all: {array: MemoryCondition, maxItems: 64}, any: {array: MemoryCondition, maxItems: 64}}}
    EvidenceRef: {type: object, required: [collection, record], properties: {collection: {ref: Binding}, record: {ref: U64}}}
    Evidence: {type: object, required: [collection, record], properties: {collection: {ref: Binding}, record: {ref: U64}, vector: {type: array, items: {type: number}, minItems: 1}, text: {type: string, maxLength: 4096}, payload_base64: {type: string, format: byte}, metadata: {ref: Properties}}}
    Assertion: {type: object, required: [subject_id, predicate, slot_key, destination_id, interval, slot_expected], properties: {subject_id: {ref: Key}, predicate: {ref: Key}, slot_key: {ref: Key}, destination_id: {ref: Key}, interval: {ref: Interval}, slot_expected: {ref: Expected}, disagreement: {enum: [reject, disputed], default: reject}}}
    MemoryItem: {type: object, required: [id, kind, content, observed_at_ms, source], properties: {id: {ref: Key}, kind: {ref: Kind}, content: {type: string, maxLength: 4096}, observed_at_ms: {ref: I64}, importance: {type: number, minimum: 0, maximum: 1, default: 0.5}, confidence: {type: number, minimum: 0, maximum: 1}, expires_at_ms: {ref: U64}, source: {ref: Source}, provenance: {array: Source, maxItems: 64}, references: {array: Ref, maxItems: 256}, evidence: {array: EvidenceRef, maxItems: 64}, metadata: {ref: Properties}, assertion: {ref: Assertion}, expected: {ref: Expected}, merge: {enum: [none, merge_provenance], default: none}}}
    Mutation: {type: object, required: [scope, idempotency_key], properties: {scope: {ref: Scope}, idempotency_key: {ref: Key}, max_references: {type: integer, minimum: 1, maximum: 256, default: 64}, deadline_ms: {type: integer, minimum: 1, maximum: 5000, default: 250}, require_complete_feed: {type: boolean, default: false}}}
    RememberRequest: {allOf: [{ref: Mutation}], x-add-properties: {items: {array: MemoryItem, minItems: 1, maxItems: 4096}, evidence: {array: Evidence, maxItems: 256}}, x-add-required: [items]}
    Ranking: {type: object, properties: {selection: {enum: [weighted, exact, mmr], default: weighted}, weights: {type: array, items: {type: number, minimum: 0}, minItems: 3, maxItems: 3, default: [1, 0, 0]}, half_life_ms: {type: number, exclusiveMinimum: 0}, future_observations: {enum: [reject, clamp], default: reject}, lambda: {type: number, minimum: 0, maximum: 1, default: 0.5}}}
    Limits: {type: object, properties: {candidates: {type: integer, minimum: 1, maximum: 1000, default: 200}, vector_components: {type: integer, minimum: 1, maximum: 4000000, default: 1000000}, evidence_bytes: {type: integer, minimum: 1, maximum: 4194304, default: 1048576}, mmr_components: {type: integer, minimum: 1, maximum: 100000000, default: 10000000}, deadline_ms: {type: integer, minimum: 1, maximum: 5000, default: 250}, max_hops: {type: integer, minimum: 0, maximum: 4, default: 2}, max_visited_nodes: {type: integer, minimum: 1, maximum: 10000, default: 1000}, max_edges_examined: {type: integer, minimum: 1, maximum: 100000, default: 10000}, max_edges_per_node: {type: integer, minimum: 1, maximum: 10000, default: 1000}, max_output_nodes: {type: integer, minimum: 0, maximum: 1000, default: 100}, max_output_edges: {type: integer, minimum: 0, maximum: 2000, default: 200}, max_output_paths: {type: integer, minimum: 0, maximum: 100, default: 20}}}
    Projection: {type: object, properties: {snippets: {type: boolean, default: true}, provenance: {type: boolean, default: true}, paths: {type: boolean, default: false}, metadata: {type: array, items: {ref: Key}, maxItems: 64, default: []}}}
    RecallRequest: {type: object, required: [scope], properties: {scope: {ref: Scope}, scopes: {array: Scope, minItems: 1, maxItems: 16}, vector: {type: array, items: {type: number}, minItems: 1}, text: {type: string, minLength: 1, maxLength: 16384}, embedding_provider: {ref: Key}, entity_seeds: {type: array, items: {ref: Key}, maxItems: 64}, kinds: {array: Kind, maxItems: 4}, filter: {ref: Filter}, as_of_ms: {ref: I64}, ranking: {ref: Ranking}, limits: {ref: Limits}, projection: {ref: Projection}, mode: {enum: [exact, approximate], default: exact}, limit: {type: integer, minimum: 1, maximum: 100, default: 20}, allow_partial: {type: boolean, default: false}}}
    Slot: {type: object, required: [subject_id, predicate, slot_key], properties: {subject_id: {ref: Key}, predicate: {ref: Key}, slot_key: {ref: Key}}}
    FactsRequest: {type: object, required: [scope, slot], properties: {scope: {ref: Scope}, slot: {ref: Slot}, as_of_ms: {ref: I64}, history: {type: boolean, default: false}, limit: {type: integer, minimum: 1, maximum: 100, default: 20}, limits: {ref: Limits}, projection: {ref: Projection}, allow_partial: {type: boolean, default: false}}}
    CorrectionRequest: {allOf: [{ref: Mutation}], x-add-required: [slot, series, slot_revision, interval, action], x-add-properties: {slot: {ref: Slot}, series: {ref: Ref}, slot_revision: {ref: U64}, interval: {ref: Interval}, action: {enum: [close, replace, remove]}, at_ms: {ref: I64}, correction_interval: {ref: Interval}, replacement: {ref: MemoryItem}, evidence: {array: Evidence, maxItems: 256}}}
    Input: {type: object, required: [id, revision], properties: {id: {ref: Key}, revision: {ref: U64}, disposition: {enum: [retain, logical_delete], default: retain}}}
    ConsolidateRequest: {allOf: [{ref: Mutation}], x-add-required: [inputs, output], x-add-properties: {inputs: {array: Input, minItems: 1, maxItems: 256}, output: {ref: MemoryItem}, evidence: {array: Evidence, maxItems: 256}, dry_run: {type: boolean, default: false}}}
    Target: {oneOf: [{type: object, required: [ids], properties: {ids: {array: Ref, minItems: 1, maxItems: 4096}}}, {type: object, required: [series], properties: {series: {ref: Ref}}}, {type: object, required: [entity], properties: {entity: {ref: Ref}}}, {type: object, required: [session], properties: {session: {type: boolean, enum: [true]}}}, {type: object, required: [scope], properties: {scope: {type: boolean, enum: [true]}}}]}
    ForgetRequest: {allOf: [{ref: Mutation}], x-add-required: [target, mode], x-add-properties: {target: {ref: Target}, mode: {enum: [logical_delete, expire]}, expires_at_ms: {ref: U64}, allow_job: {type: boolean, default: false}}}
    ReceiptRequest: {type: object, required: [scope, operation, idempotency_key], properties: {scope: {ref: Scope}, operation: {ref: Operation}, idempotency_key: {ref: Key}}}
    ErasureRequest: {type: object, required: [scope, job_id], properties: {scope: {ref: Scope}, job_id: {ref: Key}}}
    ResumeRequest: {allOf: [{ref: Mutation}], x-add-required: [job_id, revision, generation, resume_token], x-add-properties: {job_id: {ref: Key}, revision: {ref: U64}, generation: {ref: U64}, resume_token: {type: string, minLength: 1, maxLength: 4096}}}
    Receipt: {type: object, required: [id, operation, scope, sequence, digest_version], properties: {id: {ref: Key}, operation: {ref: Operation}, scope: {ref: EffectiveScope}, sequence: {ref: U64}, retry_until_ms: {ref: U64}, digest_version: {enum: [memory-request-v1]}}}
    Outcome: {type: object, required: [id, action, revision], properties: {id: {ref: Key}, action: {enum: [created, merged, noop, corrected, deleted, expired, retained_shared]}, revision: {ref: U64}, expires_at_ms: {ref: U64}}}
    Feed: {type: object, required: [mode, temporal_complete, ttl_complete, consumer_purge_acknowledged], properties: {mode: {enum: [ordinary_changes]}, temporal_complete: {enum: [false]}, ttl_complete: {enum: [false]}, consumer_purge_acknowledged: {enum: [false]}}}
    ErasureNotice: {type: object, required: [live_state_only, secure_erasure, external_copies, restore_requires_manifest, backup_history_purge], properties: {live_state_only: {enum: [true]}, secure_erasure: {enum: [false]}, external_copies: {enum: [operator_managed]}, restore_requires_manifest: {enum: [true]}, backup_history_purge: {enum: [not_verified]}, manifest_id: {ref: Key}}}
    Erasure: {type: object, required: [id, scope, target, revision, generation, state, logical_visibility, deleted_count, remaining_count], properties: {id: {ref: Key}, scope: {ref: EffectiveScope}, target: {ref: Target}, revision: {ref: U64}, generation: {ref: U64}, state: {enum: [pending, logical]}, logical_visibility: {enum: [hidden]}, deleted_count: {ref: U64}, remaining_count: {ref: U64, nullable: true}, resume_token: {type: string, maxLength: 4096}}}
    MutationResponse: {type: object, required: [receipt, replayed, outcomes, feed, remaining_work], properties: {receipt: {ref: Receipt}, replayed: {type: boolean}, outcomes: {array: Outcome, maxItems: 4096}, feed: {ref: Feed}, remaining_work: {ref: U64, nullable: true}, state: {enum: [logical, pending, retention_set]}, erasure: {ref: Erasure}, erasure_notice: {ref: ErasureNotice}}}
    ReceiptResponse: {type: object, required: [result], properties: {result: {ref: MutationResponse}}}
    ErasureResponse: {type: object, required: [erasure, erasure_notice, feed], properties: {erasure: {ref: Erasure}, erasure_notice: {ref: ErasureNotice}, feed: {ref: Feed}}}
    PlanResponse: {type: object, required: [dry_run, user_operations, encoded_bytes, conflicts, remaining_work], properties: {dry_run: {enum: [true]}, user_operations: {ref: U64}, encoded_bytes: {ref: U64}, conflicts: {type: array, items: {ref: ErrorDetail}, maxItems: 256}, remaining_work: {ref: U64, nullable: true}}}
    EvidenceView: {type: object, required: [collection, record], properties: {collection: {ref: Binding}, record: {ref: U64}, snippet: {type: string, maxLength: 4096}}}
    Path: {type: object, required: [nodes, edges], properties: {nodes: {type: array, items: {ref: U64}, maxItems: 5}, edges: {type: array, items: {ref: U64}, maxItems: 4}}}
    Scores: {type: object, required: [R, D, I, S], properties: {R: {type: number}, D: {type: number, nullable: true}, I: {type: number}, S: {type: number}, dense_rank: {type: integer, minimum: 1, maximum: 1000}, sparse_rank: {type: integer, minimum: 1, maximum: 1000}, text_rank: {type: integer, minimum: 1, maximum: 1000}, distance: {type: number}, bm25: {type: number}, sparse_score: {type: number}, fused_rank: {type: integer, minimum: 1, maximum: 1000}, fused_score: {type: number}}}
    MemoryView: {type: object, required: [id, revision, scope, kind, content, observed_at_ms, created_at_ms, updated_at_ms, importance, status, stale, segment, evidence, scores], properties: {id: {ref: Key}, revision: {ref: U64}, scope: {ref: EffectiveScope}, kind: {ref: Kind}, content: {type: string, maxLength: 4096}, observed_at_ms: {ref: I64}, created_at_ms: {ref: U64}, updated_at_ms: {ref: U64}, importance: {type: number}, confidence: {type: number}, expires_at_ms: {ref: U64}, status: {enum: [accepted, disputed]}, stale: {type: boolean}, segment: {ref: Interval, nullable: true}, evidence: {array: EvidenceView, maxItems: 64}, scores: {ref: Scores}, provenance: {array: Provenance, maxItems: 64}, references: {array: Ref, maxItems: 256}, paths: {array: Path, maxItems: 100}, metadata: {ref: Properties}}}
    ReadMeta: {type: object, required: [scopes, as_of_ms, ttl_clock_ms, sequence, complete, truncation, limitations, mode, candidates_examined, edges_charged, vector_components, mmr_components, evidence_bytes], properties: {scopes: {array: EffectiveScope, minItems: 1, maxItems: 16}, as_of_ms: {ref: I64}, ttl_clock_ms: {ref: U64}, sequence: {ref: U64}, complete: {type: boolean}, truncation: {type: array, items: {enum: [candidates, source_scan, vector_components, evidence_bytes, mmr_components, visited_nodes, edges, degree, deadline, output, deletion_barrier]}}, limitations: {type: array, items: {enum: [historical_candidate_discovery_not_exhaustive, retained_state_only]}}, mode: {enum: [exact_enumeration, approximate, bounded_expansion]}, candidates_examined: {ref: U64}, edges_charged: {ref: U64}, vector_components: {ref: U64}, mmr_components: {ref: U64}, evidence_bytes: {ref: U64}}}
    RecallResponse: {type: object, required: [memories, meta, ranking_version, selection], properties: {memories: {array: MemoryView, maxItems: 100}, meta: {ref: ReadMeta}, ranking_version: {enum: [memory-ranking-v1]}, selection: {enum: [weighted, exact, mmr]}}}
    FactView: {type: object, required: [series, slot, slot_revision, destination_id, interval, status, stale, evidence], properties: {series: {ref: Ref}, slot: {ref: Slot}, slot_revision: {ref: U64}, destination_id: {ref: Key}, interval: {ref: Interval}, status: {enum: [accepted, disputed]}, stale: {type: boolean}, evidence: {array: EvidenceView, maxItems: 64}, provenance: {array: Provenance, maxItems: 64}}}
    FactsResponse: {type: object, required: [facts, meta], properties: {facts: {array: FactView, maxItems: 100}, meta: {ref: ReadMeta}}}
    Competing: {type: object, required: [id, interval, revision], properties: {id: {ref: Key}, interval: {ref: Interval}, revision: {ref: U64}}}
    ErrorDetail: {type: object, properties: {field: {ref: Key}, limit: {ref: U64}, requested: {ref: U64}, competing: {array: Competing, maxItems: 256}, receipt_lookup_required: {type: boolean}, code: {type: string}}}
    MemoryError: {type: object, required: [error], properties: {error: {type: object, required: [code, message], properties: {code: {type: string}, message: {type: string}, details: {ref: ErrorDetail}}}}}
```

`x-add-properties`/`x-add-required` mean flatten the named common Mutation fields
and listed fields into one closed object, not allOf with an incompatible closed
base. Expand nullable refs using the repository's OpenAPI dialect, retaining
explicit null. All named scalar enums have type:string except literal booleans.
U64/I64 maximum ranges, finite numbers and decoded-byte budgets require runtime
validation. Filter is AND(all) AND OR(any); absent/empty groups are true; exists
forbids value, other operators require it; ordered comparisons require matching
integer/float/string type. Max 64 conditions total, no nesting or raw graph text.
Absent kinds means all four; empty kinds rejects. If scopes is supplied it
replaces the single scope as the exact read allowlist and must contain scope.
Sources default provenance to [source], references/evidence/metadata to empty;
no secret source redaction fallback may change identity. References to new
bundle IDs use revision "0" as a request-only sentinel; retained revisions must
be nonzero. ReceiptResponse.result has replayed=true. Mutation remaining_work
is "0" when known finished and null only when unknown. Erasure outcome arrays
are empty for bulk targets; counts/job state carry progress instead.
Read meta truncation/limitations are unique arrays sorted lexically.

### Protobuf additive manifest

Append these new messages/service to package `vlite.v1`. No existing service,
message, field, enum or feed operation changes. No tag reuse: **every new message
below additionally declares `reserved 100 to 199;`**, omitted only to keep the
manifest legible. Tags 1-99 not listed are unassigned, not reusable for a different
meaning once published; never allocate protobuf's 19000-19999 reserved range.
String enum fields use exactly the closed JSON vocabularies above (unknown
values reject); numeric absence uses optional, not scalar zero as a default.
Repeated fields represent empty arrays; nullable values use optional absence.
All sizes/defaults/conditional fields match JSON, not protobuf's implicit defaults.

```proto
service Memory {
  rpc Remember(MemoryRememberRequest) returns (MemoryMutationResponse);
  rpc Recall(MemoryRecallRequest) returns (MemoryRecallResponse);
  rpc FindFacts(MemoryFactsRequest) returns (MemoryFactsResponse);
  rpc CorrectFacts(MemoryCorrectionRequest) returns (MemoryMutationResponse);
  rpc Consolidate(MemoryConsolidateRequest) returns (MemoryConsolidateResponse);
  rpc Forget(MemoryForgetRequest) returns (MemoryMutationResponse);
  rpc GetReceipt(MemoryReceiptRequest) returns (MemoryReceiptResponse);
  rpc GetErasure(MemoryErasureRequest) returns (MemoryErasureResponse);
  rpc ResumeErasure(MemoryResumeRequest) returns (MemoryMutationResponse);
}
message MemoryScope { string agent = 1; optional string user = 2; optional string session = 3; }
message MemoryEffectiveScope { string tenant = 1; string agent = 2; optional string user = 3; optional string session = 4; }
message MemoryExpected { oneof check { bool absent = 1; uint64 revision = 2; } }
message MemoryInterval { optional int64 from_ms = 1; optional int64 to_ms = 2; }
message MemoryRef { string id = 1; uint64 revision = 2; }
message MemorySource { string source_id = 1; string source_kind = 2; optional string uri = 3; optional string transformation_id = 4; }
message MemoryProvenance { MemorySource source = 1; string content_digest = 2; }
message MemoryScalar { oneof value { string string_value = 1; int64 integer_value = 2; double float_value = 3; bool boolean_value = 4; } }
message MemoryCondition { string field = 1; string op = 2; MemoryScalar value = 3; }
message MemoryFilter { repeated MemoryCondition all = 1; repeated MemoryCondition any = 2; }
message MemoryEvidenceRef { string collection = 1; uint64 record = 2; }
message MemoryVector { repeated float values = 1; }
message MemoryEvidence { string collection = 1; uint64 record = 2; MemoryVector vector = 3; optional string text = 4; optional bytes payload = 5; map<string, MemoryScalar> metadata = 6; }
message MemoryAssertion { string subject_id = 1; string predicate = 2; string slot_key = 3; string destination_id = 4; MemoryInterval interval = 5; MemoryExpected slot_expected = 6; optional string disagreement = 7; }
message MemoryItem { string id = 1; string kind = 2; string content = 3; optional int64 observed_at_ms = 4; MemorySource source = 5; optional double importance = 6; optional double confidence = 7; optional uint64 expires_at_ms = 8; repeated MemorySource provenance = 9; repeated MemoryRef references = 10; repeated MemoryEvidenceRef evidence = 11; map<string, MemoryScalar> metadata = 12; MemoryAssertion assertion = 13; MemoryExpected expected = 14; optional string merge = 15; }
message MemoryMutation { MemoryScope scope = 1; string idempotency_key = 2; optional uint32 max_references = 3; optional uint32 deadline_ms = 4; optional bool require_complete_feed = 5; }
message MemoryRememberRequest { string binding = 1; MemoryMutation mutation = 2; repeated MemoryItem items = 3; repeated MemoryEvidence evidence = 4; }
message MemoryWeights { double relevance = 1; double recency = 2; double importance = 3; }
message MemoryRanking { optional string selection = 1; MemoryWeights weights = 2; optional double half_life_ms = 3; optional string future_observations = 4; optional double lambda = 5; }
message MemoryLimits { optional uint32 candidates = 1; optional uint64 vector_components = 2; optional uint64 evidence_bytes = 3; optional uint64 mmr_components = 4; optional uint32 deadline_ms = 5; optional uint32 max_hops = 6; optional uint32 max_visited_nodes = 7; optional uint32 max_edges_examined = 8; optional uint32 max_edges_per_node = 9; optional uint32 max_output_nodes = 10; optional uint32 max_output_edges = 11; optional uint32 max_output_paths = 12; }
message MemoryProjection { optional bool snippets = 1; optional bool provenance = 2; optional bool paths = 3; repeated string metadata = 4; }
message MemoryRecallRequest { string binding = 1; MemoryScope scope = 2; repeated MemoryScope scopes = 3; MemoryVector vector = 4; optional string text = 5; optional string embedding_provider = 6; repeated string entity_seeds = 7; repeated string kinds = 8; MemoryFilter filter = 9; optional int64 as_of_ms = 10; MemoryRanking ranking = 11; MemoryLimits limits = 12; MemoryProjection projection = 13; optional string mode = 14; optional uint32 limit = 15; optional bool allow_partial = 16; }
message MemorySlot { string subject_id = 1; string predicate = 2; string slot_key = 3; }
message MemoryFactsRequest { string binding = 1; MemoryScope scope = 2; MemorySlot slot = 3; optional int64 as_of_ms = 4; optional bool history = 5; optional uint32 limit = 6; MemoryLimits limits = 7; MemoryProjection projection = 8; optional bool allow_partial = 9; }
message MemoryCorrectionRequest { string binding = 1; MemoryMutation mutation = 2; MemorySlot slot = 3; MemoryRef series = 4; uint64 slot_revision = 5; MemoryInterval interval = 6; string action = 7; optional int64 at_ms = 8; MemoryInterval correction_interval = 9; MemoryItem replacement = 10; repeated MemoryEvidence evidence = 11; }
message MemoryInput { string id = 1; uint64 revision = 2; optional string disposition = 3; }
message MemoryConsolidateRequest { string binding = 1; MemoryMutation mutation = 2; repeated MemoryInput inputs = 3; MemoryItem output = 4; repeated MemoryEvidence evidence = 5; optional bool dry_run = 6; }
message MemoryRefs { repeated MemoryRef values = 1; }
message MemoryTarget { oneof target { MemoryRefs ids = 1; MemoryRef series = 2; MemoryRef entity = 3; bool session = 4; bool scope = 5; } }
message MemoryForgetRequest { string binding = 1; MemoryMutation mutation = 2; MemoryTarget target = 3; string mode = 4; optional uint64 expires_at_ms = 5; optional bool allow_job = 6; }
message MemoryReceiptRequest { string binding = 1; MemoryScope scope = 2; string operation = 3; string idempotency_key = 4; }
message MemoryErasureRequest { string binding = 1; MemoryScope scope = 2; string job_id = 3; }
message MemoryResumeRequest { string binding = 1; MemoryMutation mutation = 2; string job_id = 3; uint64 revision = 4; uint64 generation = 5; string resume_token = 6; }
message MemoryReceipt { string id = 1; string operation = 2; MemoryEffectiveScope scope = 3; uint64 sequence = 4; optional uint64 retry_until_ms = 5; string digest_version = 6; }
message MemoryOutcome { string id = 1; string action = 2; uint64 revision = 3; optional uint64 expires_at_ms = 4; }
message MemoryFeed { string mode = 1; bool temporal_complete = 2; bool ttl_complete = 3; bool consumer_purge_acknowledged = 4; }
message MemoryErasureNotice { bool live_state_only = 1; bool secure_erasure = 2; string external_copies = 3; bool restore_requires_manifest = 4; string backup_history_purge = 5; optional string manifest_id = 6; }
message MemoryErasure { string id = 1; MemoryEffectiveScope scope = 2; MemoryTarget target = 3; uint64 revision = 4; uint64 generation = 5; string state = 6; string logical_visibility = 7; uint64 deleted_count = 8; optional uint64 remaining_count = 9; optional string resume_token = 10; }
message MemoryMutationResponse { MemoryReceipt receipt = 1; bool replayed = 2; repeated MemoryOutcome outcomes = 3; MemoryFeed feed = 4; optional uint64 remaining_work = 5; optional string state = 6; MemoryErasure erasure = 7; MemoryErasureNotice erasure_notice = 8; }
message MemoryReceiptResponse { MemoryMutationResponse result = 1; }
message MemoryErasureResponse { MemoryErasure erasure = 1; MemoryErasureNotice erasure_notice = 2; MemoryFeed feed = 3; }
message MemoryPlanResponse { bool dry_run = 1; uint64 user_operations = 2; uint64 encoded_bytes = 3; repeated MemoryErrorDetail conflicts = 4; optional uint64 remaining_work = 5; }
message MemoryConsolidateResponse { oneof result { MemoryMutationResponse mutation = 1; MemoryPlanResponse plan = 2; } }
message MemoryEvidenceView { string collection = 1; uint64 record = 2; optional string snippet = 3; }
message MemoryPath { repeated uint64 nodes = 1; repeated uint64 edges = 2; }
message MemoryScores { double relevance = 1; optional double decay = 2; double importance = 3; double score = 4; optional uint32 dense_rank = 5; optional uint32 sparse_rank = 6; optional uint32 text_rank = 7; optional double distance = 8; optional double bm25 = 9; optional double sparse_score = 10; optional uint32 fused_rank = 11; optional double fused_score = 12; }
message MemoryView { string id = 1; uint64 revision = 2; MemoryEffectiveScope scope = 3; string kind = 4; string content = 5; int64 observed_at_ms = 6; uint64 created_at_ms = 7; uint64 updated_at_ms = 8; double importance = 9; optional double confidence = 10; optional uint64 expires_at_ms = 11; string status = 12; bool stale = 13; MemoryInterval segment = 14; repeated MemoryEvidenceView evidence = 15; MemoryScores scores = 16; repeated MemoryProvenance provenance = 17; repeated MemoryRef references = 18; repeated MemoryPath paths = 19; map<string, MemoryScalar> metadata = 20; }
message MemoryReadMeta { repeated MemoryEffectiveScope scopes = 1; int64 as_of_ms = 2; uint64 ttl_clock_ms = 3; uint64 sequence = 4; bool complete = 5; repeated string truncation = 6; repeated string limitations = 7; string mode = 8; uint64 candidates_examined = 9; uint64 edges_charged = 10; uint64 vector_components = 11; uint64 mmr_components = 12; uint64 evidence_bytes = 13; }
message MemoryRecallResponse { repeated MemoryView memories = 1; MemoryReadMeta meta = 2; string ranking_version = 3; string selection = 4; }
message MemoryFactView { MemoryRef series = 1; MemorySlot slot = 2; uint64 slot_revision = 3; string destination_id = 4; MemoryInterval interval = 5; string status = 6; bool stale = 7; repeated MemoryEvidenceView evidence = 8; repeated MemoryProvenance provenance = 9; }
message MemoryFactsResponse { repeated MemoryFactView facts = 1; MemoryReadMeta meta = 2; }
message MemoryCompeting { string id = 1; MemoryInterval interval = 2; uint64 revision = 3; }
message MemoryErrorDetail { optional string field = 1; optional uint64 limit = 2; optional uint64 requested = 3; repeated MemoryCompeting competing = 4; optional bool receipt_lookup_required = 5; optional string code = 6; }
```

Adapter conversion, not automatic protobuf JSON, defines JSON spellings:
MemoryScores relevance/decay/importance/score -> R/D/I/S; MemoryWeights
relevance/recency/importance <-> JSON `weights` [wR, wD, wI]; optional segment ->
null; optional remaining_count/remaining_work/decay -> explicit null where required;
payload bytes -> payload_base64; MemoryProvenance.source fields flatten alongside
content_digest. Required oneof checks must be set; absent/session/
scope booleans must be true. Repeated scopes empty defaults to [scope], but JSON
explicit empty rejects; JSON kinds empty rejects, protobuf empty means all kinds.
Publish these presence differences in generated client docs, never reinterpret
nonempty requests. Missing required nonoptional strings reject empty; required
revision/generation zero rejects, except `Ref.revision` 0 naming an ID created in
the same bundle (section 4); request timestamps that permit zero use optional.
All message presence requirements mirror the required JSON fields. Today prost
decoding discards unknown fields (`prost-derive` 0.14.4 `src/lib.rs:191`
`skip_field`; `build.rs:12-17` generates through `tonic_prost_build`), so an older
server silently ignores a newer request field. Rejecting them needs a
descriptor-driven pre-scan codec (owner decision, section 11); until then only
presence-checked fields and a versioned service guard against skew. Response
unknown fields remain ignorable.

### MCP, CLI and clients

Each MCP tool in section 4 has exact inputSchema
`{type:"object",additionalProperties:false,required:["binding","request"],
properties:{binding:Binding,request:<that operation's expanded request schema>}}`.
Register reusable definitions locally in tools/list, not remote schema refs.
This is the complete input shape; no hidden tenant/provider configuration.
Read tools have readOnlyHint=true; others false, including dry-run-capable
consolidate. Mutation tools have idempotentHint=false because replay is bounded
by receipt retention (none in A1; an A2 policy). Tools call the authenticated shared service,
never open a file to evade policy. Return the exact JSON response in structuredContent
and the same JSON text content; failures set isError=true with MemoryError,
never put a failed operation inside a success result. Preserve pending state;
do not poll/resume or interpret a tool response as further instructions.

Proposed new binary:
`vlite-memory-cli --url URL --binding NAME --token-env ENV VERB --request FILE|-`.
All flags are required except optional `--timeout-ms`; URL is an explicit authenticated server endpoint;
no --db, --tenant, --provider-url or implicit local-server mode.
Read request JSON from one file/stdin, <=8 MiB, use the section 4 request shape.
Mutation key/revisions/policies are in that JSON, not competing convenience flags.
`--timeout-ms 1..5000` (default 5000) sets only the client HTTP timeout; it never
edits the JSON `deadline_ms` (engine default 250) or its digest. A timeout is
transport ambiguity (exit 6); reconcile a mutation by receipt.
No token literal flag or automatic retry/paging/job loop. Success prints exactly
one response JSON line; pending is a successful accepted request with state pending.
Failures print MemoryError to stderr; exit 0 success, 2 usage/input, 3 auth,
4 conflict/precondition, 5 budget/deadline/cancel, 6 transport/storage ambiguity,
1 other failure. Receipt reconciliation is explicit, never a hidden resubmission.

Add `client.memory(binding)` in Python/JavaScript and `client.Memory(binding)` in
.NET, returning a binding-local typed facade. Methods map one-to-one in table order:

| Python (sync) | JavaScript (Promise) | .NET (Task, CancellationToken) |
|---|---|---|
| remember(RememberRequest) | remember(request) | RememberAsync(RememberRequest, cancellation) |
| recall(RecallRequest) | recall(request) | RecallAsync(RecallRequest, cancellation) |
| facts(FactsRequest) | facts(request) | FindFactsAsync(FactsRequest, cancellation) |
| correct_facts(CorrectionRequest) | correctFacts(request) | CorrectFactsAsync(CorrectionRequest, cancellation) |
| consolidate(ConsolidateRequest) | consolidate(request) | ConsolidateAsync(ConsolidateRequest, cancellation) |
| forget(ForgetRequest) | forget(request) | ForgetAsync(ForgetRequest, cancellation) |
| receipt(ReceiptRequest) | receipt(request) | GetReceiptAsync(ReceiptRequest, cancellation) |
| erasure(ErasureRequest) | erasure(request) | GetErasureAsync(ErasureRequest, cancellation) |
| resume_erasure(ResumeRequest) | resumeErasure(request) | ResumeErasureAsync(ResumeRequest, cancellation) |

Return the full typed response, never only hits or a deletion boolean. Types
match section 8 names/required presence; TypeScript discriminated unions cover
Target/Expected/Scalar and ConsolidateResponse. Python uses TypedDict/Literal
unions; .NET uses explicit request/response records and checked string integer
converters. Preserve U64/I64 as canonical strings in all HTTP facades; no JS Number
coercion. HTTP 202 is not an exception. Existing SDK error classes carry code,
status, request ID plus optional structured details; transport ambiguity remains
a transport error, not success. No auto-embedding, splitting, retries or pagination.
Cancellation cannot establish rollback. Embedded/C/WASM bindings are not changed
by these HTTP adapter slices and must not claim server tenant enforcement.

Every new public Rust API needs a `site/api.html` row with matching `data-api`
key; public names above are proposals, not evidence of Rust symbols.
The source scanner documents its keys and exclusion of src/server and src/bin
from Rust API scanning at `tests/api_reference.py:9-27`; that exclusion does not
exempt REST/gRPC/MCP reference entries or newly public core APIs.

## 9. Independent acceptance and mutation matrix

Current independent references are bounded logical models, not approved field
names or Rust parity: `tests/agent_memory_oracle.py:1-62` and
`tests/memory_ranking_oracle.py:1-54`. The former excludes facts/concurrency/
durability; the latter ranks an already eligible union with rational arithmetic,
not retrieval/TTL/MMR. Keep their fixtures independent of production output.
Run both --self-test normally and with Python -O; add engine tests for what they
do not model, explicit f64 tolerance/order fixtures and Gap 4 interval goldens.

| Acceptance area | Required assertions / intended-reason mutations |
|---|---|
| Wire goldens | Every route/RPC/tool/verb/facade request+response/error; max U64, negative/zero I64, absent/null/default, typed metadata, unknown/duplicate fields, every conditional union; mutate lost presence/rounded IDs |
| Scope/auth | Wrong tenant/agent/user/session, root versus tenant key, no parent fallback, session requires user, physical names, lineage mismatch, revoked/suspended keys, graph wildcard without operation, collection wildcard without graph scope; mutate scope filter OR/late ownership check |
| Atomicity/receipts | Final item/guard/slot/quota failure leaves no graph/vector/receipt/TTL cleanup; crash before/after append/sync; concurrent same key/different digest; replay after deletion (and, under A2, at expiry); mutate KV-only receipt, predicted sequence, deadline hashing, TTL refresh |
| Facts | Touching intervals, null/unbounded/i64 extremes, exclusive concurrent destinations, multivalued slots, exact-end no-op, revision guards, versioned evidence, correction versus deletion; mutate missing slot guard/from-to inclusivity |
| Recall/ranking | Formula/ties/graph-only R=0, future clamp/refusal/overflow, raw RRF versus cosine, exact candidate-only, MMR vector-only/refused blend, scope/TTL revalidation after provider work; mutate sign/order/rank/weights and removal of expiry owner check |
| Bounded plans | Every limit at default/max/max+1 and zero; input count before dedup; actual encoded frame bytes including receipt/reclaims; UTF-8/base64/transport accounting; high-degree invalid edges/duplicates; mutate off-by-one, clamp and output-only accounting |
| Partial/cancel | allow_partial false budget errors, true marked 200 with reasons; output cap versus requested limit; deadlines/cancellation never empty success; partial arrays remain ordered and authorized |
| Forget/jobs | Foreign attachments block whole small mutation; shared evidence retained; TTL equality hides records; barrier checked by all reads/writes; deterministic chunk retry, stale generation/revision, unknown remainder; mutate unauthorized detach, skipped barrier, unfenced lease, fake secure_erasure |
| Maintenance/feed | Healthy reopen reconciliation; encrypted/compacted/checkpoint/restore receipt+slot/job invariants; manifest caveat on all erasure/replay surfaces; ordinary event grouping/auth; temporal/TTL feed refusal before writes; mutate omitted notice or fabricated completion |
| Provider refusal | Zero outbound calls without registry authorization; unauthorized alias and provider response changes fail explicitly; text-only lexical recall works; no embedding default; mutate ambient-credential activation |
| Adapter parity | REST/gRPC/MCP/CLI/three SDKs agree on operation result, order, partial/pending, receipt replay, limits, authorization and exact error code; transport status differences only as specified |
| Compatibility | Old generated HTTP/gRPC clients on new server unchanged; new Memory clients on old server receive explicit 404/UNIMPLEMENTED with no fallback; legacy REST 1,000/gRPC 4,096 rerank unchanged; no old enum/tag/schema drift |

New goldens belong under `tests/fixtures/agent_memory/wire/` with input, expected
output, independent clock/view and exactly one intended defect per rejected case.
Mutations must fail for their intended reason, not because a second defect masks
them. Transport tests prove auth happens before lookup/egress; schema validation
alone is insufficient. None of these proposed tests is claimed to run here.

## 10. Delivery boundaries

All four slices wait for Gap 1 A-C, their core/API/maintenance acceptance and
this exact document's owner approval. gRPC wiring also waits for relevant Gap 12
Graph service/auth/error integration; other slices reuse its shared policy where
applicable, not a second implementation. Adapter dependencies below are actual
prerequisites, not permission to edit outside each slice's ownership.

| Slice | Exclusive files owned after approval | Required tests and independent acceptance |
|---|---|---|
| gap1d-rest | new `src/server/memory.rs`; memory-only integration in `src/server.rs`, `src/openapi.json`; reserved-resource guards in `src/server/graph.rs`, `collections.rs`, `features.rs`, `webhooks.rs`; new `tests/server_memory.rs`, wire fixtures; relevant REST `site/api.html` rows | Core A-C accepted; exact schema reviewed; atomic receipts/reopen, guard/quota/scope isolation including raw-route bypass attempts, every error/byte bound, provider-off, feed refusals, caveats; independent reviewer signs all sections 1/9 applicable cases |
| gap1d-grpc | new `src/server/memory_grpc.rs`; additive Memory proto/service wiring and raw-gRPC reserved-resource guards in `proto/vlite/v1/vlite.proto`, `src/server/grpc.rs`; new `tests/grpc_memory.rs`; gRPC reference rows | After shared REST core and Gap 12 policy; regenerate clients, descriptor/tag/reserved-range golden, presence/status/trailer parity, old-client compatibility, message-size bounds; independent parity acceptance |
| gap1d-mcp-cli | new memory tool support under `src/bin/tool_support/`, memory-only `src/bin/vlite-mcp.rs` registration, new `src/bin/vlite-memory-cli.rs`, binary declaration in `Cargo.toml`, `docs/MCP.md`, CLI docs and tool reference rows; dedicated adapter tests | After REST contract; remote auth/strict schema/size caps, stderr/exit/pending/replay goldens, no direct-file bypass/automatic loop, old nine-tool compatibility expectations updated deliberately; independent tool/CLI acceptance |
| gap1d-clients | `clients/python/vlite_client/__init__.py`, `clients/python/test_client.py`, Python README; `clients/javascript/index.js`, `index.d.ts`, `test.mjs`, README; `clients/dotnet/VLite.Client/Client.cs`, test Program.cs; `clients/README.md` | After REST; full typed unions/precision/errors/cancellation/pending, old methods unchanged, new-server/old-server fixtures, no automatic key/retry/provider policy; independent acceptance per language |

The current nine-tool guide assertion is `tests/agent_docs.py:10-17`; update it
only when the new tool inventory is implemented, not as part of this proposal.
Coordinate shared integration files serially; no slice may overwrite another's
accepted changes. Each checklist additionally requires relative links to resolve,
`python tests/api_reference.py` after public APIs land, and the four documentation
checks below. Slices do not change production activation/format/benchmark gates.

## 11. Owner decisions and remaining open items

The memory model/recommendations are already accepted (Gap 1 section 9); the
following choices concern only this exact serving schema and need explicit review.

| Decision | Options | Recommendation in this document | Evidence |
|---|---|---|---|
| Wire inventory | Approve nine methods here / narrower first schema / revise naming | Approve the complete inventory, including explicit consolidation and erasure reconciliation, before adapters | Sections 4 and 8 |
| Scope grants/root | Scope tenant is `Tenant.id`, with a reserved `@root` sentinel for untenanted principals / tenant-bound principals only / a startup-configured root tenant ID / client-selected tenant | Exact startup principal grants, `Tenant.id` as scope tenant, `@root` sentinel; reject client tenant; later persisted grants need a separate schema | `src/server/tenants.rs:47-52,241-245,424`; `@root` appears nowhere in `src`; `src/server/graph.rs:22-24` |
| Reserved-resource authorization | Guard raw access to new memory resources / separate graph per scope / allow raw bypass | Explicit new-binding raw_access=false policy across raw graph/record/feed adapters; operator-only broad opt-in, no ordinary-resource policy change | `src/server/graph.rs:22-24`; section 6 |
| Request identity | Body key only / header+body alias | Body-only idempotency_key across adapters; the core owns the `v1:` digest, with byte goldens from core tests before adapter code | Section 3; Gap 1 section 2 |
| Receipt retention | A2: binding-level mandatory window 1..2592000000 ms / implicit default / indefinite receipts; keep or drop A1's hashed per-request `receipt_window_ms` | Binding-level explicit window with advertised `retry_until_ms`, dropping the per-request digest input once A2 lands; manifest retention and restore procedure independently explicit | A1 receipts never expire; expiry arrives with A2 (Gap 1 section 2) |
| New memory recall caps | 1,000 everywhere / preserve transport-dependent legacy depth in new methods | 200 default, 1,000 ceiling everywhere for memory only; old Search unchanged | `src/server.rs:3032-3054`; `src/server/grpc.rs:1135-1153` |
| Presence/compatibility | Descriptor-driven pre-scan codec rejecting unknown request tags / additive-only versioned service (a field that changes scope, filter or policy meaning ships in a new version) / ignore plus presence-checked fields | Versioned service plus presence-checked fields now; add the pre-scan codec only if the owner requires rejection; publish empty-list protobuf differences | `prost-derive` 0.14.4 `src/lib.rs:191`; `build.rs:12-17` |
| Feeds | Ordinary incomplete events with refusals / block all memory until complete feeds / add events now | Ordinary events with explicit temporal/TTL incompleteness; require_complete_feed rejects; new events need another exact review | `src/server/graph.rs:85-108`; `src/property_graph/feed.rs:270-285` |
| New wire safety bounds | Adopt section 4 / revise after bounded-work review | Adopt provisionally, including the bounds section 4 marks New and the 4,096-byte content cap (stricter than A1's chunked content); consolidation inputs stay capped at A1's 64-entry provenance limit until the consolidation slice raises it; qualify actual accounting before release; no performance/size guarantee | Section 4 limits table, Basis column |
| SDK integer shape and errors | Canonical strings / language-native divergent integer JSON | Canonical strings and full typed envelopes, no lossy JS IDs or deletion booleans | Section 8 `U64`/`I64`; JavaScript Number is exact only to 2^53 |
| Oversize behaviour | Keep today's transport behavior / body-limit layer plus gRPC interceptor so every over-cap request returns a memory code / raise transport caps | Keep transport behavior; reserve `memory_request_too_large` and `memory_response_too_large` for caps the adapter checks; the gRPC halves of the 413 rows stay unreachable | `src/server.rs:60,742,1658-1662`; `src/server/grpc.rs:22-25,98-99`; tonic 0.14.6 `src/codec/decode.rs:193-197`, `src/codec/encode.rs:188-192` |
| Mutation deadline | A1's absolute `deadline_ms` with no default / adapter computes it from a relative budget / require an explicit budget | Adapter computes the absolute deadline from the JSON budget (default 250 ms, ceiling 5,000 ms, the Graph read values), excluded from the digest; vocabulary follows the central wire note | `src/property_graph/engine/limits.rs:93,104`; Gap 1 section 2 |
| Core accessors | Crate-private accessors in the core slice / adapter-side key derivation / public Rust API | Crate-private accessors for the receipt ID and, if kept, a revision pin on item references; the adapter never recomputes a receipt key; no public Rust API or `Error` variant without a separate owner decision | A1 returns `receipt_revision` and `commit_sequence` but no receipt ID, and item references are bare IDs (Gap 1 section 2) |
| Error codes | Reuse existing codes only / add `memory_*` codes with no existing equivalent | Reuse `invalid_json`, `invalid_input`, `precondition_failed`, `busy`, `poisoned`, `internal_error`; keep only the memory-specific `memory_*` codes of section 6; no public `Error` variant | `src/server.rs:1523-1525,1589-1591,1658-1662`; `src/server/grpc.rs:181-196` |

Open implementation acceptance: final Rust type/error names from A-C; binding
configuration encoding and safe generation rotation without losing retained
receipt lookup; exact digest byte goldens; bounded manifest cleanup and restore
reconciliation (these four block gap1d-rest); adapter field-number review against
the final core types (blocks gap1d-grpc); provider
snapshot-conflict mapping inherited from Gap 8. These are blockers to the relevant
implementation exit, not permission to invent success defaults. No unresolved
item changes a gate or enables a provider.

Documentation checks for this proposal: `python tests/release_docs.py`,
`python tests/content_budget.py`, `python tests/agent_docs.py`, `git diff --check`.
Only this file is added; no source, schema, proto, site, test or policy is applied.
