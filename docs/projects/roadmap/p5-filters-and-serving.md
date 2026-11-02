# P5: filters and serving

Status: implementation design; wire changes remain proposals. Delivery step 2 and the exact-planner half of
step 3 are implemented in embedded Rust, independently reviewed and integrated, but unreleased
([below](#implemented-step-2-unreleased-reviewed)). The remaining explicit approximate filter-aware ANN
half of step 3 is a private, gated slice, independently accepted and integrated but unreleased
([below](#step-3-ann-status-private-accepted-unreleased)).
Audit citations refer to base `d530b4c`; integration decisions follow the shared contract.
Open means not verified here or not yet decided.

## Scope and rules

P5 owns filter acceleration, serving formats, and metadata compression policy. The roadmap assigns these to P5
(`docs/projects/roadmap/next-major.md:17`).
F7 targets REST parity; F8 keeps the filtered-search lead and adds filter-aware ANN
(`docs/projects/performance/storage-goals.md:52-53`).
S5 targets metadata and attachments at least three times smaller on typical JSON
(`docs/projects/performance/storage-goals.md:35`). These are goals, not results.

Keep JSON as the default. Keep `src/openapi.json` frozen. The roadmap requires approval before changes
(`docs/projects/roadmap/next-major.md:82-83`).
This document proposes contracts only. It does not change the proto. Run benchmarks only at the major release, per
`docs/projects/performance/storage-goals.md:124-125`. No measurements are claimed.

The [P3 design](p3-compact-vectors.md) specifies guarded vector-column blocks,
a bounded vector accessor and lazy, generation-owned borrowed records.
The [P4 design](p4-compact-graph.md) specifies CSR checkpoint blocks.
These are not implemented. The [shared contract](compact-storage-contract.md) owns format, tags, and directory code.
P5 requests interfaces from that contract; it allocates kinds only inside its own `0x0400-0x04FF` range and does not fork it.

## Today's filter path

| Area | Checked behavior | Evidence |
|---|---|---|
| Values | String, i64, finite f64, bool, GeoPoint; no null or array variant | `src/metadata.rs:21-27`, `src/metadata.rs:74-94` |
| Conditions | Equality, numeric range, geo radius; conjunction only | `src/metadata.rs:54-69`, `src/metadata.rs:320-346` |
| Bounds | At most 64 fields, 65,536 encoded bytes, 64 conditions | `src/metadata.rs:7-9` |
| Types | Integer and float compare separately; range endpoints are inclusive | `src/metadata.rs:235-240`, `src/metadata.rs:323-328` |
| Missing | Missing fields fail equality, range, and geo conditions | `src/metadata.rs:321-346` |
| Declarations | At most 16 indexes per collection; unique field names | `src/collection.rs:68-83` |
| Index kinds | Equality, numeric range, or both | `src/metadata_index.rs:20-29` |
| Postings | BTreeSet IDs; typed equality keys; zero signs normalize | `src/metadata_index.rs:79-90`, `src/metadata_index.rs:119-146` |
| Geo | Geo equality keys exist; radius conditions have no index estimate | `src/metadata_index.rs:128-145`, `src/metadata_index.rs:265-267` |
| Persistence | Declarations persist; postings rebuild on open and declaration changes | `src/metadata_index.rs:3-8`, `src/metadata_index.rs:312-338` |
| Updates | Postings track committed replacement, metadata edits, and deletes | `src/collection.rs:218-229`, `src/collection.rs:234-252` |
| Lookup | Pick smallest indexed bound; intersect other affordable postings | `src/metadata_index.rs:362-406` |
| Correctness | Postings are a superset, never the final predicate | `src/metadata_index.rs:3-8` |

Exact search uses indexed candidates only when lookup fits half the record count; otherwise it scans all (`src/lib.rs:3368-3387`).
It checks expiry and `metadata::matches` before scoring (`src/lib.rs:3642-3650`).
Candidate IDs absent from the record map are ignored (`src/lib.rs:3502-3513`).
The exact result order is distance, then ascending ID (`src/lib.rs:3464-3467`).

`auto` defaults to an exact limit of max(4,096, 2% of record count)
(`src/planner.rs:15-32`, `src/planner.rs:46-50`).
It chooses ExactIndexed within that bound, otherwise ready FilteredAnn or ExactScan.
Unfiltered queries use ready ANN or ExactScan (`src/planner.rs:129-179`). Inner product has no ready ANN plan (`src/planner.rs:95-102`).
These counts use `records.len()`, not a query-time TTL-adjusted cardinality
(`src/planner.rs:91-94`). Estimates can therefore include expired rows.

Today's ANN is not simply unfiltered top-k followed by a metadata filter.
Base-layer traversal allows nonmatching nodes as bridges and keeps eligible nodes.
Eligibility checks current metadata, expiry, and buffered replacements
(`src/hnsw.rs:818-872`).
Selective or empty filters may visit every reachable node (`src/hnsw.rs:822-824`).
Buffered records are filtered and scored separately (`src/hnsw.rs:1023-1063`).
Reranking scores selected graph candidates, not all matching records
(`src/hnsw.rs:913-995`). Exact scores do not prove exact top-k recall.

REST dispatches exact, approximate, ready, auto, and hybrid paths
(`src/server.rs:2891-3030`).
The response is JSON; explain can include work counters and a plan
(`src/server.rs:3052-3073`).
Post-ranking is over the selected candidate list, not an exhaustive correction
(`src/server.rs:3031-3049`, `docs/API.md:140-141`).
P2's loader rebuilds derived indexes and remains eager according to its plan
(`docs/projects/roadmap/p2-directory-store.md:116-122`).
No lazy P3 implementation is assumed from that plan.

## Proposed segment statistics and bitmaps

Use immutable summaries per collection, segment, field, and type. Publish them atomically with the segment generation.
Keep the existing 16 declared-index limit initially. Do not index every field.

| Summary | Proposed meaning |
|---|---|
| Counts | Physical rows, present rows, missing rows, null rows, and per-type rows |
| Numeric bounds | Separate inclusive i64 and finite f64 min/max; empty marker when absent |
| Other types | Boolean counts; string equality dictionary; GeoPoint equality only initially |
| Presence | Missing means no key. Null means an explicit null value, never a missing key |
| Current null count | Zero: current values have no null variant (`src/metadata.rs:21-27`) |
| Arrays | Unsupported, not flattened. Null/array syntax and predicates remain Open |
| Floating keys | Normalize signed zero; reject nonfinite values as today (`src/metadata_index.rs:82-90`, `src/metadata.rs:74-77`) |
| Range pruning | Skip only if a same-type interval cannot overlap the requested bounds |
| Unknown coverage | Return unknown, retain the segment, and scan; never infer an empty result |

A mixed-type field needs multiple summaries. Do not cast integers into float bounds or use string ordering for numeric ranges.
Future arrays need explicit any/all and empty-array semantics before indexing.
Future null tests need oracle changes before null bitmaps can answer them.
For conjunctions, intersect available supersets; recheck every condition afterward.
Geo radius initially retains all rows not excluded by other safe conditions.

Use segment-local row ordinals, not truncated u64 IDs. Start with sorted sparse lists and dense bitsets.
Roaring is an alternative if its dependency and representation costs justify it.
Choose the smaller encoded sparse-list/dense-bitset representation initially;
Roaring requires measured justification at release qualification, not an assumed gain.
Keep a stable ordinal-to-ID/revision mapping in the pinned generation.

Visibility is latest-version selection AND not deleted AND (no expiry OR expiry > query time). Mask before scoring; recheck before emitting.
Capture one generation and one query clock for the whole request.
Expired rows may remain in physical summaries until merge; that only weakens pruning.
Never persist a TTL bitmap as timeless truth. Store expiry data and derive its mask.
An active-log overlay must include new matches and suppress older versions, including metadata edits, deletes, and TTL edits.

Persist sealed statistics/postings as optional versioned blocks. Rebuild mutable indexes from replay; never serialize pointers.
Validate coverage, row count, generation, schema fingerprint, lengths, and checksum.
A missing or unsupported optional block disables that accelerator with diagnostics.
Corrupt authenticated storage remains a storage error, not a silent empty posting.
The shared contract distinguishes unsupported/stale derived data from corruption;
unsupported or stale data is reported and rebuilt, never used to prune.

Bound bytes per collection, query, rebuild task, and shared cache, including dictionaries, scratch, masks, and decompression.
When an acceleration budget is exhausted, use a streaming exact scan and report it.
Do not truncate candidates. Bound rebuild concurrency and evict derived caches first.
Open: byte limits and cache eviction policy; the 16-index cap alone is insufficient.

## Filter-aware search and the exactness boundary

The existing exact filtered result must equal brute-force filter plus exact search at the same generation, clock, metric, stored values, and tie order.
`metadata::matches` remains the correctness oracle (`src/metadata.rs:320-346`).
No deleted or expired record may appear. This is separate from ANN recall.

| Selectivity or state | Proposed execution |
|---|---|
| Proven empty | Return empty only after safe pruning plus current-version visibility |
| Small candidate set | Exact scoring over the bitmap, with predicate rechecks |
| Medium candidate set in exact mode | Exact-score every eligible row; no ANN seeding dependency |
| Broad or unknown in exact mode | Exact streaming scan |
| Missing/stale ANN | Exact bitmap or scan; do not build ANN on the strict query path |
| Explicit approximate mode | Bitmap-assisted eligible heap; nonmatching bridge traversal; bounded widened ef |

ANN cannot remove unvisited eligible rows from exact mode without a proven distance bound; none is proposed here.
Widening ef, reranking, or returning k hits cannot prove exact top-k. Strict fallback remains exhaustive.
This limits strict-path speedups to safe pruning and exact scoring optimizations.

Existing explicit ANN and `auto` behavior is approximate where it selects ANN
(`src/planner.rs:129-179`, `src/hnsw.rs:878-911`).
Do not silently relabel those results exact or change existing methods.
Use the existing exact and approximate APIs, not a new strict-mode API.
Approximate results remain outside the exact top-k equality guarantee.
Both modes must satisfy the predicate and visibility rules. Auto may still choose
ANN as today; its choice does not turn approximate results into exact results.

For approximate search, widen ef deterministically to a cap. Choose work using eligible count and bitmap density, not density alone.
On exhaustion or shortfall, offer exact completion; never label partial work complete.
Approximate search guarantees predicate validity, not a universal recall floor. Measure recall against the strict oracle at release.
Merge per-segment top-k after version resolution; include the mutable overlay.
Strict ties follow distance then ID. Fix seed, segment order, and work budgets.
Open: approximate reproducibility across changed segment layouts or CPU kernels.

## Competitor comparison

Primary documents below were checked 2026-10-30; performance claims are not imported.

| Comparison | Evidence | Design consequence |
|---|---|---|
| Qdrant filterable HNSW | [Filterable HNSW](https://qdrant.tech/course/essentials/day-2/filterable-hnsw/) documents payload-aware links and cardinality-driven scans | Retain selectivity-driven exact fallback; adding links needs measured benefit |
| Weaviate ACORN | [Filtering](https://docs.weaviate.io/weaviate/concepts/filtering) describes allow lists, multi-hop traversal and matching entrypoints | Test low-correlation filters and graph connectivity; do not treat validity as exact recall |
| LanceDB prefilter | [Metadata filtering](https://docs.lancedb.com/search/filtering) distinguishes prefiltering from post-filter shortfalls | Apply predicates before accepting candidates, not only after top-k |
| Milvus filtered search | Unverified: primary documentation retrieval failed | No Milvus-specific behavior is assumed by this design |

None of these precedents establishes VLite exact top-k equivalence or a speedup.

## F7: binary responses

gRPC already implements Search through the shared search core
(`src/server/grpc.rs:1063-1122`).
The proto already carries uint64 IDs, double distances, and hybrid hits
(`proto/vlite/v1/vlite.proto:263-295`).
It supports exact, ready ANN, and hybrid, but no auto enum or explain response (`src/server/grpc.rs:411-418`, `proto/vlite/v1/vlite.proto:292-295`).
The missing work is HTTP binary negotiation and typed response parity, not a new RPC.

| Choice | Proposed benefit | Cost or limitation |
|---|---|---|
| Existing protobuf/gRPC | Reuse current typed hit encoding and transport | Auto/explain parity needs additive proto review |
| Protobuf over HTTP | Small typed unary response without JSON Value conversion | Needs a documented message and MIME/version contract |
| Arrow IPC stream | Columnar batches for large or projected results | More framing and schema work for small top-k responses |

Propose `Accept` on both search routes after approval. Absent Accept or wildcard keeps JSON; explicit unsupported types return 406.
Use `application/vnd.apache.arrow.stream` for Arrow if selected.
Open: protobuf MIME type, message identity, schema version, and client support.
Keep request JSON unchanged. Return Content-Type and Vary: Accept. Serialize typed hits without a JSON round-trip.
Arrow v1 proposal: non-null uint64 ID and float64 distance in rank order.
Hybrid output needs score and nullable distance/BM25/rank columns, not fake zeros.
Keep metadata/payload projection out of v1; current vector hit output is ID/distance
(`src/server.rs:2766-2773`, `src/server/grpc.rs:564-574`).
Bound output bytes/batches; release borrows after encoding. Preserve authorization, errors, cancellation, and request IDs.
Detect validation errors before sending a stream; specify midstream failure behavior.
OpenAPI proposals only: response media types, schemas, 406, Vary, and examples.
Proto proposals only: additive auto/explain parity; never reuse existing field tags.
No format is presumed faster than JSON until F7 release qualification.

## S5: compression boundary

Current metadata encoding writes keys and typed values directly
(`src/metadata.rs:132-165`). S5 covers attachments too; bitmaps alone cannot meet it.
P5 should own codec policy and validation; shared storage owns block framing.
Propose field/string dictionaries, boolean bit-packing, integer delta/FOR, and bounded zstd blocks for metadata and opaque bytes.
Do not lossy-compress numbers or assume opaque bytes are JSON. Keep raw blocks if compression expands them; decode identically.
Open: codec selection, block sizes, encryption integration, and attachment ownership.
Bound decompression and cache memory; preserve typed equality and byte round trips.
S5 remains a P5 slice. If deferred, mark it unmet and obtain a roadmap decision; do not redefine the goal.

## Required storage-contract interface

| Requested interface | Required guarantee |
|---|---|
| Generation pin | Own mapped/decoded data until all borrowed query records are released |
| Segment descriptor | Collection, generation, schema/capabilities, row count, stable ordinal mapping |
| Visibility view | Latest revision, tombstones, expiries, and active-overlay coverage at one snapshot |
| Bounded vector accessor | P3-compatible exact stored values and explicit errors; no unbounded materialization |
| Metadata/attachment accessor | Borrowed typed values or bounded decode; preserves oracle and raw byte semantics |
| Optional derived-block reader | Coverage/version/checksum status distinct from authoritative storage corruption |
| Derived-block writer | Atomic publication with its source generation; no partially visible statistics |
| Capability handling | Reject unsupported mandatory storage features; safely ignore optional accelerators |
| Merge/recovery hooks | Rebuild summaries and ordinal maps; retain old pinned generations; replay overlay |

No P5 file naming, tags, manifest dialect, or CSR layout. Open: finalize signatures and failure classes with the storage contract.

## Implemented step 2 (unreleased, reviewed)

Delivery step 2 and the exact-planner half of step 3 are implemented, independently reviewed and integrated
at `de2fbcd0`, including retained-capacity accounting and shared precision/P5 test instrumentation.
Acceptance covers this bounded slice, not persisted writer integration, ANN optimization or release qualification.
There is no API, wire, OpenAPI, proto, `Error` or file-format change, and nothing is persisted.

- **Summaries.** `src/compact/stats.rs` keeps one generation per collection that declares metadata indexes,
  over the declared fields only. Each block of 1024 live records in ID order records its first and last public ID,
  its row count and, per field, per-type value counts (missing = rows minus their sum; null is zero), inclusive i64
  and finite f64 min/max with signed zero normalized, the booleans seen, and string and GeoPoint equality
  dictionaries of at most 32 values and 1024 value bytes (16 per point). A fuller dictionary answers "possibly present".
- **Pruning.** A block is skipped only when one condition proves that no row can match: equality by type and value,
  ranges only against the interval of their bounds' type. Geo radius, undeclared fields and bounds that validation
  rejects are unknown and never prune. The remaining IDs are a superset; `exact_search` rechecks each with
  `metadata::matches`, the latest record and its expiry at the query clock, and orders by distance, then ID.
- **Where.** Exact filtered search (`Database::search_in_at`) uses summaries only when postings give no candidates;
  `search_auto` uses them in its final `ExactScan` arm, with the same plan and reason. They are used only when a
  block was pruned and at most half the live records remain. `index_candidates` still counts postings only;
  `records_scanned` can be lower. Grouped search, recommend, facets, geo, snapshots and ANN are unchanged.
- **Visibility.** The first filtered query that can use them derives the summaries from the loaded records
  (single file or directory image plus replay, plain or encrypted, or memory). A committed change to a declared
  field marks the row in the generation's dense superseded bitmap (by ordinal) and keeps its ID in an overlay
  that deletes leave; other changes keep the row in its block. Expiry stays a per-row deadline checked at the
  query clock; expired rows stay summarized, which only weakens pruning. Declaration changes rebuild; churn above
  max(4096, rows / 4) drops the generation and the next query derives it again.
- **Budget.** 64 MiB limits the heap one collection's summaries retain, in allocator-requested bytes: covered
  names, IDs, bitmap, block and segment lists, field lists and dictionaries at their allocated capacities (strings
  at theirs), plus the overlay at 24 bytes per ID and 168 bytes (a bound on std's B-tree nodes). It is not a cap on
  RSS, peak memory or a whole query: allocator overhead is not counted, a block is built before it is charged, and
  a query's candidate set is separate and may pass half the records (by up to one block's IDs plus the overlay)
  before the query falls back. A derivation charges the names, IDs, bitmap and lists before allocating them, then
  each block as it is built. Past the budget, at derivation or as the overlay grows, the collection scans exactly,
  logs one `warn` on target `vlite::filter` (`{"event":"filter_summary_budget","outcome":"exact_scan","rows",
  "min_bytes","budget"}`) and retries once churn passes the threshold.
- **Derived block.** Kind `0x0400`, codec version 1, holds one block summary; `src/compact/stats/codec.rs` documents
  the layout. Flags are 0 (derived). The header's source field holds the first 8 bytes of a SHA-256 over the
  covered fields and each row's ID, revision and encoded metadata. A reader serves a block only when that digest,
  the fields, the row count and the ID span match its rows. Stale or newer-version blocks are rebuilt from the rows;
  structure or checksum failures are `Corrupt`. The codec is test-only until the shared writer wiring exists, so
  loaders still skip the kind as `unknown_kind`. `0x0401` is proposed, not allocated, for persisted visibility bitmaps.
- **Tests.** `tests/filter_summaries.rs` compares indexed, unindexed and brute-force IDs, distance bits and order
  for three metrics; k 0, 7 and 10,000; ties; typed and extreme bounds; signed zero; UTF-8; booleans; geo as unknown;
  absence; invalid filters; delete/reinsert; metadata and TTL edits; explicit clocks; a merge; declaration changes;
  reopen, replay, checkpoints, conversion, compaction, encryption and memory. Summary-only queries must scan at most
  half the records. `src/compact/stats/tests.rs` replays the six oracle cases and checks an independently written
  byte-exact golden block, malformed, stale and newer-version blocks, budget exhaustion and loader skipping. The
  lib test binary's one counting allocator (`src/test_globals.rs`, shared with the precision peak tests) checks
  that the bytes charged equal the heap the summaries retain, and that the overlay bound holds. At the real 64 MiB
  budget, 16 fields of 17 one-byte strings each fit 2935 blocks and prune; one block more, or the 3900 blocks that
  initialized-only string accounting would have admitted, warn and scan.

The block size, half-records cap, churn threshold and retained-heap budget are accepted for this slice
(allocator overhead uncounted; the overlay bound follows std's B-tree node layout, checked by the tests).
Release notes must call out lower `records_scanned`. A well-formed block with a matching digest but wrong
summary is not detectable without recomputation; persisted blocks, partition-to-block binding and
multi-generation layouts wait for writer wiring.

### Step 3 ANN status (private, accepted, unreleased)

The slice targets only explicit approximate filtered search: use safe candidate supersets
to assist eligibility, retain nonmatching graph nodes as traversal bridges, and widen ef deterministically
within bounded work. Recheck full predicates, latest-version visibility and expiry at one query clock;
include buffered replacements and new matches, and suppress shadowed versions. Exact reranking of
selected candidates does not establish exact top-k recall.

Released behavior remains unchanged: no public activation, API, wire, format or persistence change;
exact search retains exhaustive correctness and distance-then-ID ordering, and existing `auto` selection
is not retuned by this slice. Missing or stale ANN, unsafe or unavailable acceleration, and exhausted
budgets must retain a correctness-preserving exact fallback with diagnostics, not silently truncate
candidates or label approximate work exact.

Independent acceptance verified these boundaries for the private gated path. Review found that budget
exhaustion during descent could return the moved-to node after checking only the entry's eligibility,
and that a zero-budget descent could return an unscored entry. The accepted correction requires a
traversal-scored, eligible best node. Its regression covers both filter shapes, budgets 0-96 and
16 queries over 2,000 records, and fails on either defect. This accepts private behavior only: no public
activation, persisted ANN or recall qualification.

Eligible-count/selectivity thresholds, ef widening and caps, traversal-work limits, and candidate,
bitmap and scratch-memory budgets are provisional implementation choices, open for release qualification;
the test-only defaults are a 4,096 eligible-candidate ef cap and at most 16,384 graph traversal
distance evaluations per query. Base ef is widened by the inverse eligible-bitmap density, capped by
the current eligible count and ef cap; the traversal budget is four times the density-adjusted width,
with a base-ef floor and bounded by the work cap. This budget excludes candidate-set construction,
exact reranking and buffered row scoring. Predicate validity is not a universal recall floor.
F7/F8/S5 remain unqualified, with no benchmark or performance claim and no change to DO-005 timing
or its quiet gate.

## Verification and release plan

The independent standard-library
[`filter_visibility_oracle.py`](../../../tests/filter_visibility_oracle.py)
checks hand-authored
[`golden.json`](../../../tests/fixtures/filter_visibility/golden.json).
Its six cases cover typed equality/inclusive ranges, conservative segment
pruning, latest-revision overlays, deletes/resurrection and explicit TTL clocks.
Geo radius is tested only as unknown pruning, not final radius filtering.
CI runs the checker with negative schema/answer self-tests. The checker is
reference coverage; the engine differential tests of step 2 are listed
[above](#implemented-step-2-unreleased-reviewed). Neither is an ANN recall
guarantee or performance qualification.

| Test family | Required assertion |
|---|---|
| Differential | Indexed/unindexed strict results equal brute-force oracle IDs, distances, and order |
| Types | Mixed i64/f64, signed zero, numeric extremes, UTF-8, boolean, geo, inclusive bounds |
| Absence | Missing fields; reject unsupported null/arrays; empty filter and empty match |
| Visibility | Delete/reinsert, metadata/TTL edits, expiry at query clock, shadowed segment rows |
| Layout | Mutable-only, sealed-only, mixed generations, merge, reopen, replay, conversion |
| Failure | Bad coverage/checksum, unknown optional/mandatory capability, budget exhaustion |
| Search | k=0, k above matches, ties, quantized candidates, stale/no index, disconnected graph |
| Serving | JSON unchanged; gRPC/binary parity; Accept/406; auth; cancellation; output limits |
| Compression | Lossless metadata and attachment round trips; malformed blocks; decode caps |

At the major release, vary selectivity, predicate correlation, skew, segment count, dimension, k, ef, cache state, updates, deletes, and expiry.
Compare exact bitmap/scan, existing/widened ANN, and strict completion. Record recall, latency, work, peak memory, bytes, and reopen cost.
Compare JSON, existing gRPC, and proposed HTTP encodings at identical result shapes.
Use typical JSON and incompressible attachments for S5; report workloads separately.
Follow the goal document's equal-recall and quiet-gate rules
(`docs/projects/performance/storage-goals.md:21-24`); publish no numbers now.

Delivery: (1) contract and oracle tests; (2) safe summaries and visibility bitmaps;
(3) existing exact planner and explicit ANN optimization; (4) lossless compression;
(5) approved binary serving; (6) major-release qualification of F7, F8, and S5.
Risks: unsafe pruning, stale visibility, ANN/exact confusion, decode memory, and schema drift. Gate correctness before performance.
Open: thresholds, memory budgets, wire/compression codecs, and whether F7 parity blocks release.
