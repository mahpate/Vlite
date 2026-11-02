# Next major release: roadmap

| Field | Value |
|---|---|
| Status | All goals and work items approved 2026-10-30. Speed P1 and the key-value API start first |
| Sources | [`storage-goals.md`](../performance/storage-goals.md) (speed and size goals, decisions 1–5), [`product-gaps.md`](../product-gaps/product-gaps.md) (gaps 1–12), Option A of [`performance-proposal.md`](../performance/performance-proposal.md) |
| Benchmarks | DO-005 runs once, after the next store version lands, under the unchanged quiet gate. No campaign runs before then |

## Work items

| Item | Track | What it delivers | Goals or exit | Size | Needs first |
|---|---|---|---|---|---|
| Speed P1: quick wins | Speed | Remove the exact-search regression, background index rebuilds, compact in-memory graph | F2, F5, S4 | S–M | Ready (phase order approved) |
| Speed P2: directory store | Speed | WAL, immutable segments and an atomic manifest, background merges, converter | F4, S6, no regressions | L | P1 (format approved) |
| [Speed P3: compact vectors](p3-compact-vectors.md) | Speed | Opt-in float16 storage, quantized codes, saved ANN graph, SIMD | F1, F3, S1, S2 | L | P2; float32 default retained |
| [Speed P4: compact graph](p4-compact-graph.md) | Speed | CSR adjacency, encoded labels, parallel bulk load | S3, S4, F6 | M–L | P2 |
| [Speed P5: filters and server](p5-filters-and-serving.md) | Speed | Segment statistics and bitmaps, filter-aware ANN, Arrow or protobuf responses | F7, F8, S5 | M | P2; shared compact contract |
| Gap 2: key-value API | Agent memory | Namespaced string keys, vector-free values, expiry, conditional writes | Pool claims, framework stores | S–M | Ready: Rust API first; OpenAPI diff reviewed before merge |
| Gap 3: graph TTL | Agent memory | Expiring nodes and edges with no dangling edges | Replaces the pool's sweeper | M | Edge-expiry rule decided (cascade); lands with or after P2 |
| [Gap 4: time-aware graph](gap4-temporal-graph.md) | Agent memory | `valid_from`/`valid_to` and "as of" reads | Fact history | M | Shared format owner; preserves accepted TTL rules |
| [Gap 1: agent memory API](gap1-agent-memory.md) | Agent memory | remember, recall, consolidate, forget | Mem0/Zep-style layer | M | Gaps 2–4 |
| Gaps 5–7 | Graph depth | [Bounded algorithms](gap5-graph-algorithms.md), Cypher subset with writes, GraphRAG ingestion | Graph parity | L | Algorithms can start independently of CSR; later adapter parity required |
| [Gaps 8 and 12](serving-contract.md) | Serving | Managed embedding/custom-reranker providers, gRPC graph query/mutation parity | SDK reach | M | Wire/API approval; existing embedding, exact/MMR and graph feeds retained |
| [Gaps 9–11](scale-out.md) | Scale-out | Network replication, object storage tier, branching | Cost and availability | L | Speed P2 |

```mermaid
flowchart LR
  P1[Speed P1] --> P2[Speed P2: directory store]
  P2 --> P3[P3: compact vectors]
  P2 --> P4[P4: compact graph]
  P2 --> P5[P5: filters and server]
  P2 --> SCALE[Gaps 9-11: scale-out]
  KV[Gap 2: key-value] --> MEM[Gap 1: agent memory API]
  TTL[Gap 3: graph TTL] --> MEM
  TIME[Gap 4: time-aware graph] --> MEM
  P5 --> BENCH[DO-005 and major release]
  MEM --> BENCH
```

## Still open

- S1's default-settings size target conflicts with the chosen float32 default.
  Whether to qualify an explicit float16 profile instead remains a release
  decision; the original criterion is not waived.
- Whether parity targets F1–F3 and F7 block the release.
- Which scale-out item comes first after P2.

## Progress

Local numbers below show direction only (see rules).

- **Shared compact storage:** the [interface contract](compact-storage-contract.md)
  reserves one guarded envelope for P3/P4/P5. Its first implementation slice
  validates framing and capabilities without enabling production compact
  writers; source codecs follow separately. Unsupported source data fails
  closed. This is not completion of the size, residency or reopen targets.
- **Speed P1, exact search (F2):** the boxed iterator is gone. At 100k×384,
  k=10 and 16 threads, p50 went from 8.2–8.3 ms to 7.1–7.9 ms, with identical
  results.
- **Speed P1, background rebuilds (F5):** opt-in
  `set_background_hnsw_rebuild` keeps serving the ready index while a buffer
  overflow rebuilds in the background. The stall has not been measured yet.
- **Speed P1, compact graph (S4):** partly done. Sorted adjacency with shared
  relationship names cut adjacency from 146 to 54 B/edge. A compact sorted
  property map (a B-tree allocated a ~630 B node even for one property) cut
  edge records from 761 to 265 B/edge. Retained memory for the smoke graph
  (10k nodes, 100k edges) fell from 101.6 MB to 37.9 MB, about 379 B/edge
  against the ≤100 B/edge goal; the rest waits for CSR storage in P4.
- **Gap 2, key-value:** the embedded Rust API is done
  ([storage features](../../STORAGE-FEATURES.md#key-value-store)): namespaces,
  ordered prefix lists, expiry, compare-and-set and fencing tokens, with no
  format change. REST and gRPC wait for the OpenAPI review.
- **Speed P2, directory store:** P2a done ([plan](p2-directory-store.md),
  [storage features](../../STORAGE-FEATURES.md#directory-store)). A database
  can be a directory of write-ahead logs and a checkpoint image listed by an
  atomic manifest. Background checkpoints bound reopen replay (S6) and keep
  every record's revision, so compare-and-set and fencing tokens survive.
  Cursors below the floor return `cursor_before_history`. Converters go both
  ways. P2b is implemented: the image and sealed logs are memory-mapped, and
  an image stores records as blocks and graphs as snapshots that load in
  parallel and in bulk instead of by replay. F4 is not measured yet; the
  DO-005 run qualifies it. Loading is still linear in the image size (see
  the plan's limits).
- **Gap 3, graph TTL:** the embedded Rust API is done
  ([property graphs](../../GRAPH.md#expiry-ttl)). Nodes and edges take
  deadlines; an expired node is hidden with all its edges (cascade), and an
  edge can also expire on its own. Graph writes, compaction and checkpoints
  remove expired entities as one computed entry, so the 4,096-operation
  limit is unaffected. Files that do not use expiry keep their encoding.
  REST, gRPC and MCP wait for the OpenAPI review.
- **Gap 4, temporal graph:** private model and persistence slices A and B
  are implemented, independently reviewed and integrated, but unreleased
  ([plan](gap4-temporal-graph.md)). They preserve atomic writes, TTL and
  revisions across replay, feeds, export and maintenance. Explicit copy-out
  leaves the source untouched and creates a new lineage requiring replica
  reseeding. Private read slices C1 and C2 are implemented and independently
    reviewed: fixed validity/TTL selectors govern ID/key reads, node scans,
    neighbors, bounded timelines, traversal, retrieval expansion and paging.
    Public activation and production qualification remain outstanding;
    production builds still refuse the temporal capability.
- **Speed P3, compact vectors:** the [design](p3-compact-vectors.md) keeps
  float32 as the storage default and makes float16 explicit. It reuses the
  existing scalar-u8/reranking and ANN snapshot mechanisms, with columnar
  source access and compact persisted topology. A rotated one-bit codec is
  an opt-in candidate, not an unmeasured new default. The private binary16
  conversion and primary-dense codec are implemented and reviewed, behind
  a test-only activation gate. Private precision-descriptor publication and
  transport validation are also implemented and reviewed. Private commit-time
  canonicalization is now independently reviewed and integrated: complete
  batch validation precedes copying, and immediate and reopened values agree.
  Float32 remains unchanged. Private complete-record checkpoints are
  independently accepted: payload/metadata sit beside unchanged dense columns,
  with named/sparse histories and graph attachments preserved in the complete
  image. Item 4's storage-owned framing/writer, capability-table and
  replay/maintenance/backup wiring is test-gated and independently accepted,
  not production qualification. The
  [API review](p3-precision-api.md) still holds public enablement pending
  complete read/write/maintenance integration. The first exact-SIMD slice
  is implemented, reviewed and integrated, but unreleased: portable and AVX2
  kernels preserve exact scoring, ordering and cutoff behavior. DiskANN
  neighbor scoring, MMR pairwise scoring and the GPU searcher's CPU
  re-score now use the same batched exact path, with unchanged results and
  cache statistics. The NEON kernel is implemented, independently reviewed
  and qualified by native ARM CI. The remaining P3d lifecycle and performance
  qualification are still outstanding. No
  production compact source writer is enabled; F1/F3/S1/S2 qualification
  remains outstanding.
- **Speed P4, compact graph:** the [design](p4-compact-graph.md) is ready:
  immutable CSR checkpoint blocks, a bounded update overlay, encoded labels
  and properties, and deterministic parallel import. The private, test-only
  source codec preserves IDs, keys, revisions, deadlines, ordinary typed
  node/edge properties, label postings with private scan and intersection
  checks, and per-node attachments with graph-local owner uniqueness; its CSR
  is checked against the accepted logical oracle. Reviewed schema declarations
  now round-trip losslessly, with engine snapshot loading checking conformance
  and rebuilding exact/range indexes. This does not persist or serve native
  compact indexes. The private codec now round-trips retained temporal edge
  series with 0–256 segments, signed/open bounds, typed segment properties and
  the edge's identity, revision and deadline, rebuilding the equivalent
  tag-65 snapshot entry. Temporal-section membership distinguishes an empty
  series from an ordinary edge. Ordinary edge records remain on the existing
  path apart from the payload version. Private decoded-CSR ID/key and adjacency
  reads are independently accepted, including TTL and selected temporal
  properties; production query execution is not wired to CSR. Shared framing,
  checkpoint/read integration, mutation overlay, parallel import and S3/S4/F6
  qualification remain outstanding. Native schema enforcement, cross-graph
  owner validation and columnar property storage also remain outstanding.
  Independently reviewed logical references cover committed-overlay
  identity, non-reused IDs and expiry precedence, typed property snapshots
  with exact integer and float bits, label postings and scans, and
  node-to-record attachments. A reviewed schema reference covers declarations,
  snapshot conformance and single-condition exact/range lookups; these are
  test evidence, not an implemented Rust overlay or byte format. The byte
  budgets are estimates, not measured results.
- **Gap 5, bounded algorithms:** S1 and S2 are implemented and reviewed in
  the embedded Rust API, but unreleased ([plan](gap5-graph-algorithms.md)).
  They provide TTL-aware local scopes, degree counts and hop-constrained
  weighted paths under shared work and memory limits. For those algorithms,
  incomplete extraction or computation returns no analytic values. S3
  PageRank and deterministic label propagation are implemented and reviewed,
  also unreleased. Their incomplete results may retain the last full iteration
  or sweep, never a partial one. S4 sampled betweenness is implemented, reviewed
  and integrated with independent reference fixtures; interrupted computation
  retains only completed sources. S4 is also unreleased. CSR parity and serving
  exposure remain separate work.
- **Speed P5, filters:** safe in-memory summaries and visibility tracking,
  including exact-planner fallback, are implemented, independently reviewed
  and integrated, but unreleased ([plan](p5-filters-and-serving.md)).
  Exact results retain full predicate and expiry checks. The per-collection
  64 MiB limit covers retained allocator-requested summary heap, not RSS,
  peak or whole-query memory; exhaustion falls back to an exact scan.
  The explicit approximate filter-aware ANN slice is private and gated,
  independently accepted and integrated but unreleased: candidate-assisted
  eligibility, nonmatching traversal bridges and bounded ef widening retain
  predicate/visibility checks and exact fallback, without exact top-k recall
  guarantees. Released exact results and `auto` selection remain unchanged.
  Thresholds and work/memory budgets are provisional and open for release
  qualification. Persisted writer integration, public activation,
  compression, serving and F7/F8/S5 qualification remain outstanding; no
  performance claim or change to DO-005 is made.
- **Serving design:** [serving](serving-contract.md) has implementation
  boundaries and acceptance matrices, not shipped capabilities. Approximate
  search does not acquire an exact top-k guarantee. Serving schemas remain
  proposals, not approved wire changes.
- **Further design proposals:** [agent memory](gap1-agent-memory.md) defines
  the embedded operations and their prerequisites; [scale-out](scale-out.md)
  compares branching, object storage and replication. Both received source
  and first-party-reference review. Neither is an implemented capability;
  the proposed scale-out order still needs a decision.

## Rules for every item

- Existing `.vlite` files keep opening; format changes arrive only with P2 and
  its converter.
- `src/openapi.json` stays frozen until an OpenAPI change is approved, so new
  features land as embedded Rust APIs first.
- Exact search results stay identical; recall and latency are compared at equal
  settings.
- Local micro-benchmarks check direction only. Claimed gains come from the
  major-release DO-005 run.

## Why key-value leads the memory track

Independent research reviewed the storage interfaces of about 15 agent-memory,
GraphRAG and orchestration systems (LangGraph, LlamaIndex, Mem0, Zep/Graphiti,
Letta, Microsoft GraphRAG, LightRAG, Cognee, CrewAI, AutoGen, Semantic Kernel,
the OpenAI Agents SDK, the MCP memory server, Haystack and Temporal).

- A namespaced key-value store with expiry and conditional writes sits under
  most of them, and is the cheapest to build on VLite's existing records.
- An append-only log or queue overlaps VLite's change feed; task claims fit
  conditional key-value writes instead.
- A document store is mostly covered by typed metadata filters and can follow
  as vector-free collections.
- Conditional writes alone do not make leases safe; claims also need fencing
  tokens.
