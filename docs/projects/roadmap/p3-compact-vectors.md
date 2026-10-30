# Speed P3: compact vectors

**Status:** design, not implemented or performance-qualified. Builds on
[P2](p2-directory-store.md); goals are
[F1/F3/S1/S2](../performance/storage-goals.md). The precision choice is resolved
below; S1's conflicting default-size acceptance criterion is not waived.
Shared framing, capability allocation and rollout follow the
[compact-storage contract](compact-storage-contract.md); source codecs remain
disabled until their complete read, write and maintenance paths are qualified.

## Precision decision

Keep **float32 as the default stored precision**. Add an explicit per-collection
float16 choice for the compact format. Existing files, collection settings,
`Record` values and exact-search calls must not silently lose precision.
Float16 conversion is lossy even when a later search is exhaustive: rescoring
cannot recover discarded input bits.

This follows the separation in
[Qdrant's datatype documentation](https://qdrant.tech/documentation/manage-data/vectors/#datatypes):
float32 is the default, an explicit datatype selects smaller original vectors,
and quantization creates an additional representation. Similarly,
[pgvector](https://github.com/pgvector/pgvector#scaling) distinguishes changing a
table to `halfvec` from quantizing its index and reranking candidates.
These are architectural precedents, not VLite performance evidence.

**S1 remains unresolved as written.** Its approved target is <=1,700 B per
768-dimensional vector at default settings. Float32 components alone occupy
3,072 B. A float16 profile has 1,536 B of components; adding a 96 B code gives
1,632 B before IDs, norms, graph links, record metadata, framing or WAL.
That subtotal is not an achieved disk budget. Measure the compact profile
separately, but do not use it to mark the original default-settings S1 complete.
Changing that qualifier remains a release decision. F1/F3/S2 and the
exact-default invariant are unchanged.

## Reuse the existing engine

P2 maps immutable files but eagerly decodes `RecordMap`
(`src/records.rs`, an `Arc<BTreeMap<u64, Arc<Record>>>`). Each `Record` owns a
`Vec<f32>`. Merely writing half-precision bytes or mapping the log again cannot
meet S2 while that full heap copy remains.

Reuse `quantize.rs` and `hnsw.rs`: scalar-u8 traversal, exact reranking,
`AnnOptions`, a default rerank multiplier of four, and background rebuilds
already exist. `ann_snapshot.rs` already validates persisted HNSW caches;
`disk_ann.rs` provides source-bound disk indexes and bounded read caches, but
its plaintext sidecars do not support encrypted databases and its build still
uses the ordinary in-memory index. None of these alone completes P3.

Retain squared-L2 and cosine ANN support. Do not silently add approximate
inner-product support or change the existing error for an unsupported ANN
metric. Exact search continues to support its existing metrics.

## Source storage and reads

Add versioned, bounded vector-column blocks to a compact checkpoint format,
with a mandatory format capability that old builds reject before replay or
torn-tail repair. Do not reinterpret existing record or snapshot bodies.
The exact tag/flag allocation must be covered by codec compatibility tests
before any compact writer is enabled. Keep the existing P2 manifest as the
publication authority, not a second catalog.

Each immutable collection generation records dimensions, stored precision,
sorted public IDs, record revisions, optional expiry, byte offsets and
checksums. Dense components are aligned little-endian float32 or float16;
payload/metadata stay separate so scoring does not decode them. Preserve
named vectors, sparse vectors, model history, graph attachments and their
existing invariants. Introduce primary dense columns first, then reuse the
accessor for named dense vectors before calling P3 complete.

Canonicalize float16 input before validation/commit becomes visible, not at a
later checkpoint. Apply existing finite-value/dimension validation, reject
half-precision overflow rather than silently produce infinity, use a specified
round-to-nearest-even conversion, and test subnormals, underflow and signed
zero. An immediate read and a reopened read must return the same canonical
values. Exact distance over float16 data means exact scoring of those stored
values, not equivalence to the original float32 input.

Mutable WAL records form a bounded overlay; an ID's newest committed version
wins, with deletes and expiry applied before candidates are returned. Existing
batch ordering, conditional revisions and graph/vector atomicity are retained.
Checkpoint publication pins reader generations and switches the base only
after durable installation. No query waits for index construction.

Internal scans and reranking use a shared vector accessor that returns mapped
float32 data or decodes float16 into bounded scratch. They must not call a
legacy iterator that materializes every `Record`. Preserve public borrowed
`get` and read-snapshot signatures through stable, generation-owned decoded
records, materialized only when those APIs need them. Such borrowed records
cannot be evicted while references may exist: account for this retained cache
and document that reading every record eagerly can consume full-vector memory.
Do not silently evict, leak references or claim this memory is outside S2.

Read snapshots must still outlive their database, with unchanged visibility and
retention limits. They therefore pin needed file generations as well as overlay
state. Unmap before Windows deletion. Keep a file-read path where mappings are
unavailable; neither path may hide an I/O or authentication failure.

## Derived ANN representation

Store graph topology as bounded flat offset/neighbor arrays with checked
ordinals, not per-node heap allocations. Bind every persisted index to its
source generation, collection ID, content digest, precision, dimensions,
metric, build configuration, quantizer version and seed. Reuse snapshot
validation and explicit stale/corrupt rejection outcomes; never serve stale
codes as current data or treat a corrupt source block as a cache miss.
A missing or rejected derived index may rebuild through the existing
background-index path, with its status visible to callers.

Prototype a dimension-preserving, rotated one-bit codec with full stored-value
reranking for the compact profile. A 768-dimensional sign code is 96 B;
rotation metadata, norms/correction terms and graph links are additional.
Avoid padding to 1,024 stored sign bits while claiming a 96 B code. Version
and persist every transform/scoring parameter so reopen is deterministic.
Do not label a simplified sign codec as a complete RaBitQ implementation.

Start from the existing four-times rerank width and bounded `ef_search`, not
a new unbounded search API. Candidate generation is approximate; reranking
can only reorder candidates already retrieved. The
[Qdrant quantization guide](https://qdrant.tech/documentation/manage-data/quantization/)
specifically warns that low-dimensional data and insufficient candidates can
lose recall despite rescoring. Keep scalar-u8 and unquantized traversal as
explicit alternatives. One-bit traversal is not a new unconditional default
until VLite's equal-recall acceptance measurements justify it.

For encrypted files, use authenticated bounded blocks and a bounded decrypted
cache, never a plaintext vector sidecar or spill file. Existing plaintext
disk-ANN sidecars are not an encryption escape hatch. Persisted ANN data is
derived but still sensitive and must use the same confidentiality boundary.

## SIMD and compatibility

Keep the current float32 exact scoring and tie order as the oracle. New exact
kernels must preserve its f64 accumulation semantics; do not enable FMA or
reassociate sums merely to improve timing. Approximate traversal kernels may
use different arithmetic within their explicit mode, followed by canonical
reranking. Use runtime CPU dispatch with a portable scalar path, and retain
no-default-feature and WASM builds. Do not require unsupported instructions.

Precision is chosen at collection creation. Changing a populated collection's
precision uses explicit conversion into a new destination, keeping the source.
A lossy conversion creates a new lineage; it is not P2's identity-preserving
format-only conversion. Revisions, cursors and compatibility behavior must be
documented and tested rather than inherited accidentally from backup.

Preserve existing constructors, `Record` and `AnnOptions` fields. Extend
persisted settings in a versioned form with explicit public API baseline
review. REST, gRPC, MCP, CLI and bindings do not acquire a new wire field
without their corresponding contract approval; `src/openapi.json` is frozen.

## Delivery slices and acceptance

1. **P3a, precision and codec:** float32-default/float16-opt-in settings,
   canonicalization, guarded vector blocks and explicit conversion. Keep the
   existing eager path as a compatibility oracle; do not claim S2 yet.
2. **P3b, bounded source access:** column access, overlay integration, lazy
   borrowed-record compatibility, snapshot/file lifetime handling and encrypted
   reads. Exercise exact/filtered/hybrid and graph-attached record paths.
3. **P3c, persisted compact ANN:** flattened topology, source-bound codes,
   background rebuild integration and the opt-in rotated-code prototype.
   Reuse existing scalar-u8 and index-cache behavior rather than duplicate it.
4. **P3d, dispatch and qualification:** verified SIMD kernels, complete
   conversion/backup/restore/key-rotation coverage and major-release acceptance.

For each slice, add golden/malformed codec tests, immediate-versus-reopen
equivalence, old/new format rejection, atomic failure, expiry/conditional
writes, deterministic rebuild, stale cache, concurrent checkpoint/snapshot
retirement, and bounded cache/overflow tests. Floating-point edge cases and
stable exact ordering are correctness gates, not approximate recall tests.

The independent conversion oracle is
[`tests/compact_float16_oracle.py`](../../../tests/compact_float16_oracle.py),
with bit-pattern fixtures in
[`float16-cases.json`](../../../tests/fixtures/compact/float16-cases.json).
It checks signed zero, subnormal/normal transitions, nearest-even ties and
overflow against Python's binary16 encoder. Its CI self-test rejects a
deliberately corrupted expected bit. This validates the reference answers,
not the future Rust codec: P3a must consume the same fixtures.

Major-release measurements retain the unchanged DO-005 host/quiet policy.
F1 requires p50 <=1.0 ms at 100k x 768 and recall@10 >=0.95; F3 requires
build time <=2x zvec at the same dimensions. S2 includes codes, graph,
resident mapped pages, overlays, decoded-record and decrypted caches, and
other retained index state; it must stay <=25% of the comparable float32
baseline. Report build peaks separately. Mapping the full vector file does
not make resident pages free: use bounded windows or file reads when needed,
and include warm steady-state residency rather than only an empty-cache run.

For disk accounting, include source columns, persisted codes/topology,
IDs/revisions, metadata, framing, encryption and steady-state WAL. Report
temporary checkpoint duplication separately. No claimed gain, release gate,
or S1 profile exception follows from the design estimates.
