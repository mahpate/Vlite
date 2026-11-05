# Speed P3: compact vectors

**Status:** in progress, not performance-qualified. Private float16 conversion,
the primary dense rows codec, descriptor publication and commit-time
canonicalization are implemented, independently reviewed and integrated behind
a test-only gate ([P3a progress](#p3a-progress-private-dense-rows-codec)), as is
the private P3b column accessor; no production setting, writer or reader
enables them. The P3b engine seam (pinned generation, base-plus-overlay view,
effective rows and the bounded read workspace) is integrated through slice 3
and lane B, test-gated and compiled only under `cfg(test)`; its 0.7.1 freeze
is in [p3b-seam-freeze.md](p3b-seam-freeze.md). Builds on
[P2](p2-directory-store.md); goals are
[F1/F3/S1/S2](../performance/storage-goals.md). The precision choice is resolved
below; S1's conflicting default-size acceptance criterion is not waived.
Shared framing, capability allocation and rollout follow the
[compact-storage contract](compact-storage-contract.md); source codecs remain
disabled until their complete read, write and maintenance paths are qualified.

**P3b column-access status:** a private, unreleased, test-only accessor is
implemented in `src/compact/catalog/column.rs`, independently reviewed,
mutation-checked and integrated; no production path calls it.
It checks each dense or complete-record body once, validates metadata during
the eager record-block check, exposes borrowed ID, revision, expiry, payload
and encoded-metadata columns, and searches IDs within a block and across a
validated block directory. Vector access currently decodes the complete block
into an explicit scratch cache for both precisions;
the cache is byte-bounded to decoded float32 vector bytes, requires enough room
for one decoded block, and evicts least-recently-used blocks by a logical
counter with the block key as a deterministic tie-breaker. An undersized
budget returns the existing `InvalidQuery` error. Cached shape mismatches (element count,
dimensions or precision) return `InvalidQuery` without changing the cache.
Same-shape key collisions remain the engine's responsibility: integration
must use generation-aware keys. Unknown block versions retain the existing
`UnsupportedFeature(FLAG_COMPACT)` refusal rather than being reclassified
as corruption. The accessor has no unsafe
vector alignment or casting; adding a safe aligned borrow is deferred. Its
owned-versus-mapped parity test uses a test-only memory map with a documented
safety condition. Accessors borrow only the caller-supplied verified body
slice; generation ownership, overlays and `Database` integration are separate
engine layers tracked below. This accessor makes no performance, size, or
release-qualification claim.

**P3b slice 0 status — private and unreleased; independently accepted and
integrated.** Independent acceptance replayed fail-first mutations of block
boundaries, checksum and padding checks, the locked once-cell recheck,
membership, column extents, capture isolation, sign-preserving reads and
gate defaults/restoration; each failed its targeted test and passed once
restored. The test-only source module retains each checked record block's
on-disk frame and the row/column extents produced by the same `ImageCheck`
validation pass. `format::load_inner` constructs the `Arc<CompactGeneration>`
only after the image end block validates. The normal loader's resident
`RecordMap` remains the oracle for subsequent private integration slices;
slice 0 itself adds no `Database`, `Collection`, `ReadSnapshot`, `Chain`,
mutation path or public API wiring. Release builds do not compile the source
module or its gate.

The standalone view supports only the following surface:

| Surface | Available in the test-gated point view | Refused or absent |
|---|---|---|
| Record reads | `get` / `get_at` with explicit `now_ms`; expiry hides when `expiry <= now_ms`. | No implicit clock and no mutable access. |
| Point columns | Physical `len` (including expired rows), `first_id`, `contains_id`, binary `row_of`, ID, revision and expiry by row. | No ordered iterator or bulk scan. |
| Record materialization | Stable `&Record` from a generation-owned once-cell, decoded at most once after successful access. | No decoded-block cache or persistent plaintext sidecar; no public `Record` change. |
| Search and collection APIs | None. | Browse, exact/ANN/hybrid/filter/group search, KV, named vectors, index access and graph attachment APIs are not wired to the view. |
| Writes and lifecycle | None. | Put, delete, metadata updates, batches, checkpoint, compaction, conversion, backup/restore, graph rewrites, feed, replica and `Database`/snapshot installation are not exposed by this slice. |
| Source backing | Loader capture uses an owned buffer; a test-only mapped copy exercises identical frame extents. Encrypted generations retain ciphertext frames and the existing cipher context, authenticating/decrypting each record read into a temporary frame. | No directory-store segment installation or mapped `Database` generation; Windows retirement remains deferred to slice 5. |
| Gate off | Existing release and default behavior is unchanged. | A compact critical source kind without the existing test activation returns `UnsupportedFeature(FLAG_COMPACT)`. |

The owned backing retains only verified on-disk frames and their immutable
directory; the directory records frame kind, extent, checksum, sequence and
original file offset, while the generation retains the cipher context when
encrypted. Plain frames have already passed footer/checksum and codec
validation before publication. Encrypted reads recheck checksum and AEAD
before decoding. The temporary plaintext frame is operation-local; the
stable decoded `Record` cell is memory-resident and is not evicted while the
generation lives. Mapped bytes, decoded cells and temporary plaintext are
separate accounting categories; no public snapshot counter is changed.
Database installation, snapshot retention and overlays are tracked in Part J;
ordered reads and lifecycle parity remain for later slices. This slice makes
no performance, size or release-qualification claim.

The engine-integration design below has been independently reviewed; owner
decisions K.1-K.6 were explicitly approved on 2026-11-02. Each code slice
still requires independent acceptance; these decisions grant no activation.

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

**S1 is qualified on the explicit float16 profile (decided 2026-11-02).** Its
original target was <=1,700 B per 768-dimensional vector at default settings,
but float32 components alone occupy 3,072 B. A float16 profile has 1,536 B of
components; adding a 96 B code gives 1,632 B before IDs, norms, graph links,
record metadata, framing or WAL. That subtotal is not an achieved disk budget:
the major-release run measures the float16 profile against 1,700 B. Float32
stays the default. F1/F3/S2 and the exact-default invariant are unchanged.

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
2. **P3b, bounded source access:** the private column accessor, slice 0's
   test-gated pinned point-read source and slices 1a, 1b, 2 and 3 (resident and
   compact record dispatch, installation, the committed-mutation overlay,
   ordered browse and exact search) are integrated, as is lane B's part of
   slice 4, with its fix-forward and replayed mutation ledgers (see the progress notes and the [0.7.1 seam freeze](p3b-seam-freeze.md)).
   0.7.1 adds lane A's record consumers (A4, test-gated, with replayed mutation ledgers).
   Remaining: the bounded exact scan (lane E4; lane A's draft is cherry-picked), lane C's
   server and MCP consumers, and slice 5 (snapshot and file lifetime across
   checkpoint, maintenance, replica and encrypted lifecycle). Exercise
   exact/filtered/hybrid and graph-attached record paths.
3. **P3c, persisted compact ANN:** flattened topology, source-bound codes,
   background rebuild integration and the opt-in rotated-code prototype.
   Reuse existing scalar-u8 and index-cache behavior rather than duplicate it.
   The [detailed design](p3c-compact-ann.md) is independently reviewed and its
   owner decisions were approved 2026-11-02; no Rust ANN code is integrated
   (only the independent Python reference), and code starts only after P3b
   engine integration is accepted.
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
deliberately corrupted expected bit. This validates the reference answers;
the private Rust codec consumes all 80 fixtures (see below).

## P3a progress: private dense rows codec

Implemented in `src/compact/vectors.rs`, compiled for tests only. This is the
first P3a step, not P3a completion or S1/S2/F1/F3 qualification. Release
readers still refuse kind `0x0100`, like every source kind, with
`UnsupportedFeature(FLAG_COMPACT)`. No writer emits it, and no public
setting, error or format change exists yet.

- **Conversion.** Float32 to binary16 rounds to nearest, ties to even, and
  keeps signed zero and subnormals; magnitudes up to 2^-25 become signed
  zero. Infinity and NaN are non-finite; finite magnitudes of 65,520 or more
  would round to infinity and are refused as overflow. Binary16 to float32
  is exact. All 80 shared fixtures pass, and every finite half and every tie
  between neighbouring halves is checked exhaustively. Float32 keeps every
  finite input bit and stays the default.
- **Block.** Kind `0x0100` version 1 is critical. Each op-16 frame has ID
  `0x0100 | partition << 16`, and its `count` is the row count. The shared
  envelope has six little-endian sections. Each starts 64-byte aligned in
  the file when the writer knows the offset; readers do not require this.

  | # | Section | Contents |
  |---|---|---|
  | 0 | Descriptor | dimensions `u32`, precision `u8` (1 float32, 2 float16), 3 zero bytes |
  | 1 | IDs | `u64` per row, strictly ascending |
  | 2 | Revisions | `u64` per row, each in `1..=S` |
  | 3 | Expiring rows | `u32` row numbers, strictly ascending, below `count` |
  | 4 | Deadlines | `u64` per expiring row |
  | 5 | Components | row-major float32 or float16, all finite |

- **Bounds.** A block has 1 to 4,096 rows, and more than one only within
  1 MiB of components. At 768 dimensions that is 341 float32 or 682 float16
  rows; at 65,536 it is 4 or 8. Readers check the body length, frame,
  envelope and version first; other versions are unsupported. They then
  check the flags, section and row counts, the descriptor against the
  collection, and every section length with checked arithmetic, all before
  reading a value. Rows are borrowed from the verified body.
- **Exactness.** The encoder refuses a component that is not already a
  stored value, so a checkpoint never rounds, overflows or stores a
  non-finite value. It also refuses wrong dimensions, unsorted IDs and
  out-of-range row counts. Canonicalization belongs before commit.
- **Scope.** A block owns IDs, revisions, expiry and the primary dense vector.
  Payload, metadata, named and sparse vectors, model history and graph
  attachments stay in their existing frames. These blocks alone never
  reconstruct a collection.
- **Tests.** The tests cover the shared fixtures and exhaustive ties,
  bit-exact float32 round trips at all 64 alignments, and an independently
  written float16 golden. They also cover malformed blocks, encoder
  refusals, bounds, authentication before decoding encrypted blocks, and
  release readers refusing an otherwise valid image.

## P3a progress: commit-time canonicalization (private, unreleased)

Implemented, independently reviewed and integrated in `src/lib.rs`
(`validate_stored`, `round_stored`), reached only through a privately opted-in
precision store. This is P3a item 2's
canonicalization, not P3a completion. No public API, setting, error, format
or wire field changes.

- **Writes.** Every primary and named dense value written to a float16
  collection is checked during the existing validation, allocating
  nothing, and then rounded by the accepted binary16 conversion before
  encoding: in place, in the copy a float32 write makes too, or for a batch
  or import chunk in one copy made only after every row and the batch
  limits pass. This covers single, conditional and expiring puts, ordered
  batches (duplicates in order), import chunks, graph vector entries, named
  versions, whole named vectors and re-embedding batches. The log and the
  installed state hold the same values, so
  immediate borrowed reads, exact scores and reopened stores agree bit for
  bit; replay, the change feed and export still verify every value. Float32
  collections, sparse values, metadata-only writes and query vectors are
  unchanged, and float32 writes copy nothing extra.
- **Refusal.** A value that rounds to float16 infinity is `NonFiniteVector`,
  exactly where a non-finite value is: after a wrong width, before payload,
  metadata and preconditions. Nothing commits: no record, revision, import
  marker, expiry or graph change. Imports report it under the existing
  reject and skip policies. A retry is identified by its raw submitted rows,
  so different rows with the same float16 values are a different chunk.
- **Tests.** `src/compact/catalog/tests/canonical.rs` writes the shared
  goldens (ties, subnormals, signed zero, overflow) through every path into
  float16 and float32 collections on memory, file and directory stores,
  plain and encrypted, and compares immediate, checkpointed and reopened
  bits and exact scores with the unchanged float32 oracle. It also counts
  heap bytes with the precision peak harness: a refused float16 batch
  allocates nothing, however large its valid rows, and every other float16
  write allocates and keeps what float32 does, plus one batch copy.
- **Not included.** Public precision activation, production `0x0100`
  writers and capability, the source-column writer, copy-out conversion,
  faithful compaction and upgrade (still refused before output), and
  lifecycle and production qualification. S1's default-size conflict and
  DO-005 stay unwaived.

Remaining P3a work:

1. The public precision setting, overflow error and creation/conversion API,
   under separate review.
2. Persisted settings and commit-time canonicalization, so immediate and
   reopened reads agree. Canonicalization is implemented privately
   ([above](#p3a-progress-commit-time-canonicalization-private-unreleased));
   it stays unreleased with item 1.
3. Carrying payload and metadata for compact checkpoints, so they hold every
   record. Implemented privately and independently accepted
   ([complete-record blocks](#p3a-progress-complete-record-checkpoints-private-unreleased));
   no production admission or writer is enabled.
4. A storage-owned writer, capability table acceptance, and replay,
   maintenance and backup handling. Implemented privately and independently
   accepted
   ([storage wiring](#p3a-progress-storage-owned-writer-private-unreleased));
   no production writer or capability is enabled.
5. Explicit conversion, plus old/new format rejection: implemented privately
   in `maintenance::convert_precision` and
   `src/compact/catalog/tests/convert.rs`, independently accepted and
   integrated, but unreleased. An independent mutation that skipped rounding
   named dense versions failed three conversion tests until restored.
   Crate tests only, behind the existing precision gate: selected collections
   convert float32 to float16 (lossy) or widen exactly, using the commit-time
   binary16 routine. Complete V5 sources are exclusively locked read-only;
   unsupported flags, open imports and retained dense overflow refuse before
   output. Absent file/directory destinations, plain or same-key encrypted,
   get a complete flag-32 image with new lineage and head=floor=max(head,1).
   Numeric revisions, allocation high-waters, closed imports, expired state,
   named/sparse histories and graph attachments survive; old log history does
   not. Old-lineage feeds/replication fail and replicas must re-bootstrap;
   matching numeric revisions are not lineage-bound, so callers must reread.
   Release readers still reject flag 32 and compact source kinds. No public
   API, production writer/activation, P3b/P3c/P3d or S1/S2/DO-005 change.

Named dense reuse and engine-side bounded access remain P3b; only the private
column accessor is integrated.

## P3a progress: complete-record checkpoints (private, unreleased)

Item 3 is implemented and independently accepted. Its body codec remains in
`src/compact/catalog/payload.rs`, compiled only for crate tests and enabled by
a separate thread-local RAII gate, in addition to the private precision gate.
Kind `0x0101`, version 1, critical,
holds complete primary records rather than redefining `0x0100`. Its six
little-endian envelope sections are:

| # | Section | Contents |
|---|---|---|
| 0 | Descriptor | row count `u32`, reserved zero `u32` |
| 1 | Dense body | unchanged `0x0100` v1 envelope/body, same owner, partition and count |
| 2 | Payload offsets | `count + 1` offsets `u32`, from zero through payload length |
| 3 | Payload bytes | unchanged opaque record payloads, concatenated |
| 4 | Metadata offsets | `count + 1` offsets `u32`, from zero through metadata length |
| 5 | Metadata bytes | existing typed metadata encoding per row, concatenated |

The embedded dense codec owns sorted IDs, original revisions, absolute expiry
(including zero), dimensions, precision and canonical component bits. Descriptor,
frame count, offset-table lengths and dense row count must agree. Offsets must
be nondecreasing and exhaust their sections; each payload and encoded metadata
value obeys the existing record limits. The writer flushes at the dense row cap
or 1 MiB of payload/metadata, allowing one final maximum-sized record. The
prefix body cap is `2 * 1 MiB + MAX_PAYLOAD_BYTES + MAX_METADATA_BYTES + 128 * 4096`,
including nested framing, dense columns and offset tables. It is an allocation
bound, not a measured storage result. Alignment remains advisory.

The image capability table declares `0x0101` only when a retained record exists;
empty collections remain represented by the mandatory precision catalog. The
blocks replace op 14 at the existing record stage, preserve partition runs and
ascending IDs across blocks, and are followed by the unchanged op-10 named and
sparse vector data, including model history and staged re-embedding state.
Graph declarations, attachments, properties and entity allocation state remain
in op-15 snapshots with kind-5 original revisions. Import state remains kind 4.
Thus the **complete image**, not the primary-record block alone, reconstructs
every retained record and its associated state, following Decision P in
[precision publication](p3-precision-publication.md). With no retention clock it
keeps expired records too; ordinary checkpoints retain their existing expiry
filter. No source bytes, settings, op-10 or graph encodings are reinterpreted.

Every frame is authenticated before its codec runs. Envelope, dense lengths,
checked offset-table arithmetic, per-value limits and final offsets are checked
before allocating decoded record values. Truncation, trailing bytes, malformed
typed metadata, invalid dense data, count mismatches, unsupported versions,
duplicate/out-of-order IDs and invalid partition runs are refused. Full replay,
change-feed scans and log export share the same block validation. Release builds
still refuse the precision flag; even a crate test with only the older precision
gate refuses the new critical source kind.

The private tests compare immediate borrowed records, complete images and
reopened memory/file/directory stores for both precisions, plain and encrypted,
including payloads, typed metadata, vector bits, revisions, expiry, named/sparse
vectors, model history and graph attachments. Directory tests inspect the actual
published image for `0x0101` and reject an op-14 fallback. A deliberate removal of
the test-only worker-gate propagation fails that assertion; restoring it passes.
Malformed-image tests exercise all three readers, a pre-publication failure
retains the previous image plus committed WAL, and the precision peak harness
bounds decoded-record heap and verifies malformed offsets allocate no records.

## P3a progress: storage-owned writer (private, unreleased)

Item 4 is implemented behind test-only gates and independently accepted.
`src/compact/mod.rs` owns complete-record framing, writer dispatch and capability
table construction; `src/compact/catalog/payload.rs` retains only the six-section
`0x0101` v1 body encoder and checker. The writer clamps original record revisions
to `1..=floor`, matching op 14. Capability acceptance requires the private gates
and exact supported kind/version; unsupported critical entries remain refused.

Independent acceptance reran the compact, maintenance, storage and directory
suites; removing the `0x0101` capability declaration fails every transport test.
Tests cover capability acceptance/refusal and complete-record replay,
backup, restore and re-encryption. Byte-copy paths retain guarded flags and source
state; restore changes lineage, and re-encryption preserves canonical values
with fresh encryption state. Maintenance still refuses unfaithful rewrites
before output, under Decisions W/P. This is not production writer/capability
qualification, bounded source access, item 5 conversion, item 1 public activation
or lifecycle qualification. Float32 defaults, S1's open decision and S1/S2/F1/F3
remain unchanged.

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

## P3d progress: exact SIMD kernels (reviewed, unreleased)

Implemented, independently reviewed and integrated in the crate-private
`src/exact_kernels.rs`. This is the first
P3d step: verified exact kernels with runtime dispatch. It is not P3d
completion, conversion/backup/restore/key-rotation coverage or a measured
gain. No public API, setting, error, format or wire field changes.

- **Semantics.** `metric_distance` stays the unchanged oracle. A pass scores
  up to eight rows against one query, and each row owns one f64 accumulator
  lane that performs the oracle's operations in the oracle's order. Both
  components are widened (exactly), then either subtracted and squared or
  multiplied, and then added to the running sum. The sum starts from the
  standard library's empty f64 sum (-0.0 on the current toolchain), so
  zero-dimension rows match too. Multiplication and addition stay separate,
  and no kernel enables FMA. Two widened float32 values multiply exactly in
  f64, so FMA could not change inner-product, cosine or norm bits. A
  squared-L2 difference can round, however, so FMA would change those bits.
  The final metric step is the oracle's own scalar code: the inner-product
  zero rule, the cosine zero-norm result and the clamp. Without a stored
  norm, cosine folds the row's squares alongside the dot product, exactly as
  `vector_norm` does.
- **Order.** Exact scans still keep a bounded top-k ordered by distance
  (`total_cmp`), then ID. That is a total order, so scoring in batches cannot
  change the kept set or ties. Squared-L2 pruning drops a batch only when
  every row's partial sum already exceeds the current k-th distance, since
  nonnegative terms cannot bring it back. Statistics are unchanged.
- **Operands.** The reranker and named-vector search used to call the oracle
  query-first. Their vectors are validated finite. For finite values, a - b
  is the exact negation of b - a under round-to-nearest-even, and
  multiplication commutes, so scoring row-first returns the same bits. A row
  whose length differs from the query is scored by the oracle itself.
- **Dispatch.** Detected once per process. x86_64 with AVX2 uses two groups
  of four f64 lanes, transposing rows four dimensions at a time. Aarch64
  selects baseline NEON unconditionally: four groups of two f64 lanes,
  each lane accumulating one row in dimension order with separate multiply
  and add instructions. Otherwise the portable kernel keeps one scalar
  accumulator per row; that is SSE2 on x86_64 and scalar code elsewhere,
  including WebAssembly. Native aarch64 CI matrix jobs validate NEON
  bit-identity against the oracle; local Windows x86 checks do not execute
  or compile that kernel, and native validation remains an acceptance gate.
  Tests force portable and NEON on aarch64 (portable and available AVX2 on
  x86_64), including a finite squared-L2 witness that distinguishes FMA
  from separate multiply/add. The override does not propagate to Rayon
  workers; they retain runtime dispatch, so it does not prove
  forced-portable parallel execution.
- **Paths.** Exact search, filtered exact search and candidate subsets,
  including the parallel scan. These also serve hybrid, planner and
  graph-attached record retrieval. HNSW exact re-scoring (unquantized and
  scalar-u8 rerank), the buffered-record merge, `ExactReranker` and
  named-vector exact search use the kernels too. DiskANN scores each
  expansion's neighbors in bounded batches, including upper-layer links,
  then applies candidates in the original link order. Loads and visited
  checks retain their order, so the beam, ties and cache statistics are
  unchanged. A batch briefly holds up to eight decoded neighbors outside
  the cache, so transient memory can exceed the cache budget by a few
  node records. MMR pairwise scores and the GPU searcher's CPU f64 re-score
  and CPU fallback also use the batch path. The GPU f32 pass and the
  in-memory approximate traversal kernels are unchanged.
- **Tests.** Every metric, with and without stored norms, is compared bit for
  bit with the oracle under each kernel. Coverage: dimensions 0 to 67, 128,
  384, 768 and 1,536; batches of 1 to 9 rows; seeded values over about 80
  binades; signed zeros and subnormals; `f32::MAX`, `f32::MIN` and
  `f32::MIN_POSITIVE`; infinities and NaN; cancellation; zero norms; and
  mismatched lengths. Rust does not fix NaN payloads, so any NaN matches a
  NaN. End-to-end tests run every exact, ANN, merge and rerank path above on
  data with exact ties, with the per-thread dispatch limitation above. Their IDs, distance bits, order
  and statistics must match a full-sort oracle. Deliberate FMA, starting-sum
  and AVX2 reordering mutations each fail these tests. The
  `exact_kernel_fingerprint` example prints per-scenario result fingerprints
  for comparing two revisions. An ignored local timing test checks direction
  only; it is not a measurement.
  DiskANN, MMR and GPU CPU-path tests additionally compare with their
  original one-pair oracle algorithms under forced portable and detected
  SIMD dispatch, including exact ties, signed zeros and subnormals at
  dimensions 0 to 67, 128, 384 and 768 (end-to-end paths start at 1,
  because public validation rejects 0). DiskANN compares cache accounting
  with both a one-node and a full cache; invalid or unsupported public
  cases are exercised through private scoring fixtures, not new APIs.
  Stored and computed norms and reachable mismatched lengths are covered.

Native aarch64 acceptance passed on Rust 1.89.0 and stable in
[CI run 41318163085](https://github.com/mahpate/Vlite/actions/runs/41318163085)
at `7e3e56a2`, together with the full CI matrix. This qualifies the NEON
kernel, not the remaining lifecycle or performance goals.

Remaining P3d work: conversion, backup, restore and key-rotation coverage,
DO-005 measurements and major-release acceptance.

## P3b engine integration design

**Design independently reviewed; owner decisions K.1-K.6 approved
2026-11-02; the design is not implementation acceptance or activation.**
All source line references below are from setup
head `91d0e38c62dea7997b072d02118aedc45ca63e1b`. The accepted column code
and record codec are test-only at this head; release readers still refuse
compact source blocks. This design changes no code, test, fixture, format,
writer, API, policy, gate, default or release criterion.

### A. Scope and non-goals

Integrate verified primary columns with existing collection reads, writes,
snapshots, indexes and lifecycle while preserving public `Record`, exact
search and durability semantics. Float32 remains the default; float16,
temporal copy-out and compact writers remain explicit opt-ins, each behind
its own qualification. The [compact-storage contract](compact-storage-contract.md)
owns framing, registry, verification and accessor invariants; this proposal
narrows the engine seam.

No production writer, public activation, new persisted format, WAL operation,
sidecar, dependency, public error/API, policy change, or performance/size
claim is part of this design. If implementation requires one, raise a
separately reviewed sub-slice rather than widening this scope.

### B. Call-site inventory at setup head

The counts are **consumer families per file**, not raw token matches: a
cohesive set of accesses in one subsystem is one family. Test modules and
test-only compact writer/checker paths are excluded from production counts.
The inventory includes `RecordMap` storage/reads/writes and record-field
consumers; `ReadSnapshot` callers are listed with `records.rs`. Each row gives
the replacement seam and owning code sub-slice.

| Production families | File and verified locations | Current pattern | Replacement / sub-slice |
|---:|---|---|---|
| 4 | `src/records.rs:17-120,164-261,554-635` | `RecordMap` storage/iteration; snapshot clones map and expiries; borrowed point `get`; ordered browse clones records; exact snapshot scan. | `CollectionRecords` read view, snapshot-pinned generation and lazy stable `Record` cell; S1, S3. |
| 6 | `src/collection.rs:142-282,320-328,335-425` | `Collection.records` construction, derived metadata/text index rebuild, committed put/delete/metadata mutation, count/first-ID, browse/group, exact search. | Concrete base-plus-overlay source and effective ordered cursor; S1-S3. |
| 6 | `src/format.rs:348-421,466-472,690-855,2017-2082,2577-2602,2716-2826` | `LoadedCollection.records` is built by legacy ops and op-14 `BlockRun`; the complete-record supplement is test-only; count/default-map access; frame encoders and `check_frame_writes` read record fields. | Verified compact base installation and replay into overlay; stream checkpoint from effective rows; S1, S2, S5. |
| 10 | `src/lib.rs:894-918,1585-1589,1851-1870,1972-2218,2414-2450,2514-2557,2670,2874-2952,3226-3604,3698-3884` | Database counts/get/CAS and write apply; checkpoint hooks; ANN build/rebuild; ready and lazy approximate paths; exact search and parallel row scan; summary/filter candidates. | `CollectionRecords` and operation-scoped `ReadAccess`; writes update overlay only after commit; S1-S4, S5. |
| 6 | `src/hnsw.rs:27-32,314-329,472-520,595-620,949-1051,1162-1391,1495-1712` | `RecordMap` adapters, build/encode vectors and norms, mutation buffer, metadata filter, rerank and snapshot validation. | Vector/metadata accessors pinned to one generation; keep derived indexes source-bound; S4. |
| 3 | `src/rebuild.rs:205-232,325-408` | Background rebuild captures a `RecordMap`, tracks previous records, snapshots counts and builds from that state. | Rebuild job pins a `ReadView`/generation plus sequence-bounded overlay snapshot; S4. |
| 2 | `src/metadata_index.rs:319-343,417-430` | Index rebuild and summary candidate verification read `Record.metadata`. | Rebuild and reverify from effective metadata rows; never let summaries decide visibility; S4. |
| 2 | `src/text_index.rs:96-110` | Text index rebuild and tracking read `Record.metadata`. | Feed effective metadata on replay/commit and verify current visibility; S4. |
| 2 | `src/ann_snapshot.rs:255-369` | Persisted ANN digest/save and load validation iterate primary vectors and counts. | Bind snapshots to source generation identity; compact-backed save/load must stream through the accessor; S4. |
| 2 | `src/disk_ann.rs:163-274` | Sidecar build and source validation use live `RecordMap` vectors. | Use the accessor only for supported plaintext sidecars; encrypted sidecars remain unavailable; S4. |
| 4 | `src/vectors.rs:383-439,584-591,650-768` | Named-vector search checks primary-record existence; re-embedding iterates records and clones them; vector mutations require a live base record. | Separate primary existence/metadata lookup from named-vector storage; preserve owned output only where API requires it; S3. |
| 2 | `src/kv.rs:294-377` | Point probes and ordered namespace/prefix listing decode key/value data from `Record.payload`. | Point/ordered payload slice from effective row; overlay-first and TTL semantics; S3. |
| 1 | `src/hybrid.rs:159-197` | Text leg checks expiry and `Record.metadata` after postings. | Effective metadata/expiry predicate on the same captured generation and clock; S3. |
| 1 | `src/query_language.rs:410-521` | Candidate depth, metadata rechecks, ordering, facets and groups read `Record.metadata`. | Query-local metadata accessor; order/facet/group over visible rows; S3. |
| 1 | `src/gpu.rs:105-125` | GPU query preparation gathers IDs and primary vector slices. | Bounded accessor-fed staging buffer; preserve its existing size checks and CPU fallback; S4. |
| 1 | `src/compact/stats.rs:570-580` | Summary/statistics build iterates records and metadata. | Effective metadata scan; keep conservative candidate-superset semantics; S4. |
| 1 | `src/planner.rs:95,172` | Planner uses live count and metadata-index summaries over records. | Effective count plus source-aware summary view; S4. |
| 2 | `src/quality.rs:127-192` | Reports record count/resident-vector bytes and scans primary vectors. | Report source/cached/overlay accounting distinctly; use bounded access for vector scan; S4. |
| 3 | `src/server.rs:1838-1928,2029-2031,2815-2891,3238,3549-3564` | REST point/list responses copy `Record` fields; quota and index metrics count records and estimate resident vectors. | Existing public response materializes only returned records; count from view, distinguish resident cache from source rows; S3-S4. |
| 2 | `src/server/grpc.rs:614-616,919-921,2215-2228` | gRPC output copies vector, payload and metadata; batch/list route reads records. | Same `Record` compatibility materializer and effective scan as REST; S3. |
| 1 | `src/bin/vlite-mcp.rs:424-433` | MCP responses serialize payload, metadata and optionally the vector. | Materialize only response records through the shared read view; S3. |
| 1 | `src/server/features.rs:71` | Feature endpoint serializes committed put records. | Preserve response shape through the same materializer; S3. |
| 1 | `src/server/webhooks.rs:373` | Webhook event serializes committed put records. | Preserve event shape through the same materializer; S3. |
| 1 | `src/rerank.rs:140-141` | Search rerank output clones record vectors and payloads. | Keep owned output where required; resolve through one pinned read view; S3. |
| 1 | `src/server/observability.rs:59-67,292` | Live counts and resident-vector estimate. | Source-aware counts and honest cache accounting, without changing telemetry policy; S4. |
| 1 | `src/server/tenants.rs:64` | Tenant quota reads collection record count. | Effective physical-row count with overlay deltas; S2-S3. |
| 4 | `src/maintenance.rs:313-343,600-701,856-925,1014-1041,1394-1411` | Precision validation/rounding, conversion/backup/compaction walks records and fields, record-copy/rewrite paths. | Stream merge of base and overlay into staged destination; keep old source/generation until publication; S5. |
| 1 | `src/changes.rs:103-375` | Feed replays committed frames into owned `ChangeKind::Put { record }`; it does not read `RecordMap`. | Validate compact checkpoint blocks through shared image check; emit only post-floor WAL changes, never restatement rows; S5. |
| 1 | `src/property_graph/frame.rs:95-192` | `GraphRecords` validates dimensions/vector mutations and whether attached records remain live without expiry. | Reuse the collection view's `exists`/`lasting` checks at replay time; keep graph mutations atomic; S2, S5. |
| 1 | `src/property_graph/maintain.rs:75-105` | Graph copy-out validates attachments against `LoadedCollection.records` and expiries. | Effective row membership during graph rewrite; S5. |
| 1 | `src/property_graph/retrieve.rs:394,470-480` | Graph retrieval filters on metadata and projects metadata/payload excerpts. | Use the captured effective row and materialize only requested evidence; S3. |
| 0 | `src/directory.rs:165-210,399-450,730-775,855-925` | No `RecordMap` consumer; owns mapped sealed files, directory-chain installation and deletion. | Generation must pin the sealed image mapping and defer unlink after publication; S1, S5. |
| 0 | `src/compact/mod.rs:612-1264` | Production `ImageCheck` verifies frame/block runs and graph liveness; P3 complete-record writer/apply helpers are test-only. | Add concrete P3 block check/apply branch only after contract review; do not make test gates production; S1, S5. |
| 0 | `src/compact/catalog/{column.rs,payload.rs}` | Candidate accessor/checker and full-record decoder are test-only; no release engine caller. | Preserve verified-slice API and error provenance; no code change in this design. |
| 1 | `src/metadata.rs:136-200` | Existing typed metadata encoder/decoder used by public `Record` and compact payload columns. | Decode only the selected row when materializing a `Record`; malformed bytes remain `Corrupt`; S1. |

`src/property_graph/frame.rs` already has a focused `GraphRecords` contract;
reuse its semantics, not a new codec/plugin trait. The concrete engine seam
proposed below is an enum-backed `CollectionRecords`. `src/directory.rs` and
`src/compact/mod.rs` are listed despite zero direct map families because they
own image lifetime and checked block dispatch. The source scan found no record
`next_id` allocator: record IDs are supplied by callers; `collection_high`
allocates collection IDs (field `src/format.rs:436`, `src/lib.rs:920`;
allocation `src/lib.rs:2630-2647`),
and KV slots are derived by `slot_base` (`src/kv.rs:407`). Row ordinals must
never substitute for those public IDs.

### C. Generation ownership, lifetime and accounting

Use one immutable `Arc<CompactGeneration>` per checked image: it owns image
identity, pinned read-only map or file/offset backing, checked block directory,
collection layouts, row counts and stable lazy-record cells. The owned-buffer
fallback reads the same offsets. Encrypted reads authenticate before exposing
an operation-local, evictable plaintext block; never persist plaintext
sidecars. Use the concrete enum `CollectionRecords::Resident(RecordMap)` or
`Compact { base: Arc<CompactGeneration>, overlay: Arc<RecordOverlay> }`, not
an open-ended storage trait.

**Loader handoff and source seam (slice 0 prerequisite).** At the reviewed
design head, `format::load_inner` reads frame bytes and calls
`verified_frame` before dispatch (`src/format.rs:1835-2085,2654-2674`).
`ImageCheck::block` validates the complete-record codec and returns decoded
entries (`src/compact/mod.rs:1198-1257`); `BlockRun::apply` installs owned
records in `LoadedCollection.records` (`src/format.rs:2519-2603`). The
validated source extent is not retained by `Loaded`, and encrypted plaintext
is transient. Therefore the loader must explicitly hand a validated source
directory to generation construction.

During that same `ImageCheck` pass, retain immutable descriptors for each
supported record block: kind, owner/partition, on-disk extent, checksum and
AEAD context. Do not revalidate the block with a second validator to build
the directory. A generation may be published only after the complete image
end block and every source block pass. Backing is either a read-only mapped
file (non-WASM) or owned buffer using identical extents. Plain source bytes
are verified once before publication and then immutable. Encrypted generations
retain ciphertext extents and the file `Cipher` context only; each read
authenticates/decrypts into an operation-local, evictable plaintext frame
buffer, never a persistent plaintext sidecar.

Account mapped bytes separately from decoded-vector scratch and the
plaintext frame buffer; all three are bounded and included in internal
accounting, without changing public snapshot counters. The generation pins
its selected backing until its final handle drops. A mapped generation cannot
be replaced/unlinked on Windows while a view still owns the map; `Chain::install`
currently drains replaced segments and removes them immediately after durable
publication (`src/directory.rs:730-775`). Until slice 5 implements deferred
retirement, any future Database-installed generation must use owned-buffer
backing. Directory-store/segment images remain refused with
`UnsupportedFeature(FLAG_COMPACT)` for that integration. Standalone slice 0
can exercise mapped parity without installing a Database generation.

`Database`, readers and workers hold an `Arc` for their operation. A
`ReadSnapshot` pins the generation and sequence-specific immutable overlay
root, outlives `Database`, and preserves `get`, `get_at`, browse/search,
sequence, lineage and captured-clock behavior. Rebuild/checkpoint pins its
source and overlay/high-water snapshot. Publish a new generation only after
staged validation and durable manifest/file publication; maintenance retains
the old source through copy-out. Final `Arc` drop releases mappings and allows
unlink. Because `directory::Chain::install` currently drains old segments
immediately (`src/directory.rs:743-775`), deferred generation retirement must
be implemented explicitly.

Preserve `MAX_READ_SNAPSHOTS=8`, `MAX_READ_SNAPSHOT_RECORDS=100_000` and
`MAX_READ_SNAPSHOT_BYTES=128 MiB` with current rejection semantics
(`src/records.rs:123-126,201-225`); clones share the existing per-capture
logical-body lease. Account materialized `Record` cells, overlays and
float16-to-f32 expansion; report mapped and logical decoded bytes separately.
Existing `get` can materialize all rows, so full-record residency is possible
and is not S2 qualification. Capture/materialization failure, overflow,
fallback failure, stale generation or budget exhaustion must fail explicitly,
never appear as a missing row.

### D. Cache identity and invalidation

The exact logical cache key is `(generation identity, collection_id,
block_kind, partition)`. A generation-scoped cache may omit no part of this
identity; any cross-generation cache needs a live opaque generation token.
The shape guard checks element count, dimensions and precision, not byte
identity. Same-shaped collisions are prevented by the key, not the guard;
I/O/authentication errors can never become misses.

A stored block holds at most `BLOCK_COMPONENTS` = 1 MiB of components (a byte
budget; `Layout::max_rows`, `src/compact/vectors.rs`), so a decoded `f32` block
is at most 2 MiB (float16 expands twofold). Budget 4 MiB per operation for
vector scratch (room for two of the largest decoded blocks),
with deterministic logical-counter LRU. The separate plaintext frame buffer
is bounded by the contract's per-block body cap (section 4). Initially exact
compact scans are serial in one operation context, avoiding shared mutable
caches and per-worker budget multiplication. Stable borrowed-record cells
cannot be evicted while borrowed. Count scratch, cells and plaintext in
memory accounting; these are bounds, not performance claims.

Reopen always creates a fresh generation/cache. Checkpoint swaps generations
only after publication; old readers retain the prior one. Compact, precision
conversion and graph rewrite get new source identities and never reuse old
entries. Byte-identical backup may preserve lineage, but an opened restore
gets a new in-process identity. Persist no cache state; test generation
isolation and deterministic eviction across reopen, checkpoint, conversion,
backup/restore, re-encryption and concurrent snapshots.

### E. Overlay precedence, deletes, expiry and IDs

Store committed mutations in a persistent/COW ordered overlay keyed by public
record ID: `Put { revision, expiry, Record }` or `Delete { revision }`.
The WAL remains the durability authority; update the overlay only after the
existing commit succeeds. `RecordMap` resident mode remains unchanged.

| Base row | Newer overlay | Effective row |
|---|---|---|
| absent | absent | absent |
| present | absent | base, including its revision and expiry |
| present or absent | put | overlay record, revision and expiry |
| present | delete tombstone | absent; never fall through to base |
| present | expired put | absent at that query clock; never reveal older base |
| expired base | absent | absent at that query clock |

Choose the greatest committed revision, hide it when `expiry <= now_ms`, then
evaluate filters. `Collection.len` and `ReadSnapshot.len` remain physical
counts including TTL-hidden rows. Ordered scans merge ascending IDs, consume
equal base/overlay IDs, emit only the overlay winner and skip tombstones or
expired winners. Results, tie order, cursors and clocks must match resident
behavior at the same sequence/time.

Preserve batch order, duplicate-ID, conditional-revision and metadata-only
write semantics. Metadata-only updates materialize the winning base row and
install a complete overlay record. Expiry never reallocates IDs; record IDs
remain caller-supplied, graph attachments keep those IDs, collection
high-water is separate/non-reused, and KV slot reuse rules stay unchanged.

### F. Lazy `Record` compatibility

Keep public `Record { vector: Vec<f32>, payload: Vec<u8>, metadata: Metadata }`
and signatures unchanged (`src/lib.rs:773-777`). An immutable per-row
generation-owned `OnceLock<Record>` supplies stable borrows for existing
`get/get_at` APIs. On first access resolve the winning row, use bounded
vector access, copy canonical f32 values and payload, decode only that row's
typed metadata, then publish the complete record. Float16 yields the f32
represented by stored half bits; revision/expiry remain columns.

Borrowed references stay tied to a generation/snapshot, never evictable
scratch. Overlay records are immutable in the captured COW root; `BrowseHit`
and REST/gRPC outputs remain owned. Exact/ANN scoring uses borrowed vectors
without constructing every `Record`; predicates decode only required
metadata, and scoring-only paths do not decode payload/metadata.

Cells live until their generation drops because `&Record` cannot carry a
cache guard. Account actual vector, payload and metadata allocations,
including half expansion. Apply existing count/body limits before snapshot
capture; snapshots share cells but keep independent leases. Full materialized
residency is possible; `get` is not promised constant-cost or compact. No new
public cap or eviction behavior (owner decision 2).

### G. Authenticated I/O and errors

For every block: **read bounded bytes → verify footer/checksum →
authenticate/decrypt → validate envelope and codec → pin generation →
expose columns or touch caches**. `format::verified_frame` checks footer and
checksum, then maps failed AEAD auth to `Corrupt`
(`src/format.rs:2654-2674`). Header key checks precede replay
(`src/lib.rs:1441-1448`; `src/crypto.rs:300-375`). Owned reads use the same
path. Never hide I/O, authentication or codec errors as cache misses.

| Condition | Result |
|---|---|
| Unknown critical source kind/version, including nested dense version | Pass through `UnsupportedFeature(FLAG_COMPACT)` unchanged. |
| Invalid envelope, offsets, row count/range, checksum, or authenticated-frame tag | Existing `Corrupt { offset, reason }`; never repair an immutable image. |
| Missing key / wrong key / key supplied to plaintext file | Existing `EncryptionKeyRequired` / `EncryptionKeyMismatch` / `NotEncrypted`. |
| File read, seek, map fallback, or publication I/O failure | Existing `Io`/publication error; do not convert to absence or miss. |

`BlockDirectory` ranges are ordered/nonoverlapping with `rows > 0`,
`first_id <= last_id`, and overflow-safe `rows <= last_id-first_id+1`.
Before serving, reconcile block totals, collection ownership, revisions,
expiry rows and directory count. Effective physical count includes overlay
puts minus replaced/tombstoned base IDs; expired rows remain counted.
Snapshots capture counts/revisions/expiry atomically. Mismatches are
`Corrupt`, never short scans or silent truncation.

### H. Writes, checkpoint and lifecycle

**First write choices.** (1) Convert the whole compact collection to resident
`RecordMap` before mutation. This reuses current paths and is a straightforward
correctness bridge, but decodes every vector/payload and defeats bounded
source residency; it also needs a snapshot-safe source retirement rule.
(2) Keep the immutable generation and write committed full-record puts and
tombstones to an ordered WAL overlay. **Recommend (2)**: it matches the
accepted P3 source/overlay model, lets existing durability ordering remain
authoritative, and preserves old generations for readers. Do not mix the
approaches silently; overlay is the engine integration target.

- **Commit/WAL/recovery:** validate and encode through existing put/batch
  paths; append/sync as today; only then update overlay, revisions, expiry,
  vectors, metadata/text indexes and graph attachments. Replay starts from
  authenticated compact base, then applies later WAL operations in order.
  Recovery must reproduce the same overlay and high-water state.
- **Checkpoint:** pin `(base generation, overlay root, sequence, clock)`;
  build a complete staged image by ordered merge; validate it fully, sync and
  atomically publish; install a new generation and clear only the included
  overlay. Concurrent writes after the captured sequence remain in the new
  WAL. Snapshots/rebuilds can retain the old image.
- **Compact/precision conversion:** stream from the effective view; apply
  TTL at the caller's explicit/current clock; preserve current lineage,
  revision, import, named/sparse-vector and graph rules exactly. Float16
  conversion is explicitly selected and must validate all retained source
  rows before creating output. A failed stage leaves source and destination
  state unchanged.
- **Backup/restore/re-encryption:** byte-copy backup keeps encrypted frames
  and capabilities; validate source first. Restore and `restore_to` preserve
  their current lineage/cursor policy; re-encryption authenticates before
  rewriting and uses the selected destination key. No plaintext sidecar or
  spill. Each path gets source-generation lifetime tests.
- **Graph enable/strip:** copy-outs must enumerate effective base+overlay
  IDs and apply attachment/no-expiry checks at the same replay/maintenance
  points. Keep `GraphRecords` semantics, graph atomicity and explicit graph
  data-loss confirmation. These operations remain governed by their current
  opt-in and refusal rules.
- **WAL/feed/replica:** compact block frames are checkpoint restatements,
  not user change events. `changes_after` starts after the image history
  floor; feed and export validate the complete image and surface only
  committed later WAL changes. Replica application retains append-then-replay
  atomicity; source-generation replacement is installed only after replay
  succeeds.
- **Reopen:** discard process-local caches, validate/authenticate the image
  and replay WAL before exposing `Database`; old snapshots remain pinned only
  in the old process/generation. Reopen cannot revive expired winners or
  bypass unknown critical kinds.

### I. Minimal read seam for P3c, P3d and precision API

Freeze one crate-private concrete contract, not empty traits. Slice 0 first
provides a standalone point-read view over a pinned generation:
`get/get_at` at an explicit clock, physical `len`, `first_id`, `contains_id`,
and row ID/revision/expiry accessors. It has no scans, mutations, or Database
integration. Slice 1 introduces `CollectionRecords` with exhaustive
`Resident` and test-only `Compact` matches, point `get/get_at`, physical
`len/is_empty`, `first_id` and ID membership. Public borrowed `Record`
remains generation-cell-backed. Ordered/vector/payload/metadata accessors
and a `ReadAccess` pinning one clock, generation and overlay root belong to
later slices; do not add unused accessors in this point-read-only slice.

**Coordinator ruling for slice 1 (A-D).** No public signature, policy, gate,
default or release criterion changes. Every reachable public method must
have exactly one truthful classification:

| Class | Methods / behavior | Acceptance obligation |
|---|---|---|
| A: served observations and point reads | `CollectionRef::{id,name,dimensions,settings,len,is_empty,get,get_at,revision,expires_at}`; `ReadSnapshot::{sequence,lineage,dimensions,len,is_empty,get,get_at}`; Database catalog/handle observations such as `dimensions`, `format_version`, `lineage`, `is_read_only`, `is_poisoned`, `durability`, `is_directory`, `is_encrypted`, `encryption_key_id`, `has_commit_timestamps`, `unsynced_commits`, `collections`, snapshot counters and point reads (including revision/expiry observations). Internal physical membership and first-ID lookup use the same source. | Same-image resident parity, one explicit TTL clock and stable borrowed records. Infallible text-index status observes the retained derived-index state, not a new scan. |
| B: documented precondition cannot arise | `Database::cancel_hnsw_rebuild` returns false when no job is pending; starting a compact rebuild refuses. | No invented success; preserve resident cancellation. |
| C: configuration or raw committed image | `set_checkpoint_threshold` remains the documented memory/single-file no-op; directory images remain refused. `set_commit_clock` affects write timestamps, not point reads. `to_bytes` copies committed memory backing, or returns the existing file-backed `None`. | Point parity after clock configuration, exact input-byte parity and reopen parity. |
| D: unsupported fallible operations | Refuse `collection_mut` acquisition, making all `CollectionMut` operations unreachable for compact. Refuse browse/scans, every search mode, KV, named/sparse vectors, indexes/rebuilds, graph acquisition/attachment, writes/batches/metadata/TTL changes, import, checkpoint/maintenance, feed and replica operations. | `UnsupportedFeature(FLAG_COMPACT)` before state or sequence changes, including empty batches, zero-k searches and other early returns. |

If an infallible record scan/mutation cannot truthfully fit A-C, slice 1
must refuse Database installation at open rather than invent a result.
Crate-private resident iteration is fallibly dispatched; it is not a
public API classification.

**Slice 1a status: independently accepted and integrated (private,
unreleased).** The concrete enum replaces collection and snapshot record
storage; resident-only scan/mutation access returns the existing typed
refusal. Loader installation is deliberately unchanged: even with private
test activations, normal Database open still installs resident collections.
Seven dispatch regressions inject verified standalone views only after a
normal resident open, for plaintext and encrypted images. They cover point
observations, raw bytes and clock independence, representative method-level
refusals with unchanged bytes/sequence, mixed resident writes, gate-off
refusal, enum lifetime and a snapshot fixture outliving its Database.
The mixed fixture also preserves resident geo-validation error precedence
on a poisoned handle; compact refusal does not reorder that resident path.
Internal mutable-handle acquisition by ID also refuses compact records,
including the server and default-collection ANN delegates.
These fixtures are not evidence of loader installation or complete public
method coverage. Slice 1b must add the exhaustive A-D public-method matrix,
owned-source installation only after full image validation, late-image
failure atomicity and snapshot/accounting acceptance through real open.
Mapped installation and directory/segment images remain out of scope;
no activation, wire/format change, new public error or API is introduced.

Independent acceptance reran the full matrix on the assembled candidate:
strict workspace Clippy, 789 default and 835 all-feature library tests,
14 storage/graph/rerank integration targets, native and WASM no-default
checks, and API/documentation checks. Five coordinator mutations each
compiled, failed their named regression (exit 101) and passed again after
restoration: removing the rerank target guard, checking only the default
collection for whole-handle graph recall, reading compact points at clock 0,
ignoring the compact evidence refusal and replacing the evidence-target
guard with a whole-handle guard. After integration, formatting, strict
Clippy and the compact and memory suites passed in both feature modes.
Full graph recall still refuses a mixed resident/compact `Database`; only
evidence acquisition checks its single selected target. Slice 1b remains
open.

**Slice 1b status: independently accepted and integrated (private,
unreleased).** With the existing precision, records and source test gates,
real Database opens now install the owned verified source for compact
record collections (including empty collections alongside compact records).
Generation construction follows complete image validation and pending
snapshot application. There is no mapped Database installation; standalone
mapped parity tests remain unchanged. Directory/segment images refuse
before writable-directory cleanup. File and read-only opens initialize
their ordinary durability internally; public durability changes still
refuse compact handles.

Independent acceptance found that a valid collection may combine legacy
op-14 rows and compact op-16 rows: capturing one block is not proof of
complete collection coverage. The corrected candidate compares captured
row count, every ordered ID, revision and expiry with the validated resident
collection before any installation. Partial same-collection coverage
refuses with `UnsupportedFeature(FLAG_COMPACT)`; it never replaces the full
map with only the captured subset. Plaintext and encrypted regressions
cover that refusal, source-byte preservation, and an empty compact image
followed by a committed WAL suffix remaining resident and writable.

**Slice 2 status: independently accepted with notes and integrated (private,
test-gated, unreleased).** It reconciles a committed suffix over the captured
compact image as a base-plus-overlay view; images without captured compact
records retain resident dispatch, and no rows are discarded to force
installation. Independent mutation review killed 20 of 23 deliberately
failing mutations; two (A17 and B02) are documented redundant. The third
(QL1) is not strictly equivalent for a text-only query on a poisoned handle,
so slice 3 adds one poisoned-handle execute test.

The real-open A-D matrix covers plaintext and encrypted images, populated
default/named collections and empty collections. Every public receiver
method on Database, CollectionRef and ReadSnapshot is exercised; every
CollectionMut method is enumerated as D, unreachable at refused acquisition.
Associated constructors and path utilities are identified separately by
`tests/compact_method_matrix.py`, using the API inventory to detect omitted
or stale cases. Tail repair and replica acquisition refuse compact sources;
the temporal feed checks compact residency before its temporal-feature
refusal. Existing mixed-resident target behavior and whole-handle graph
refusal remain unchanged.

Snapshots pin the owned generation and stable, lazy, non-evicting Record
cells after Database drop. Eight distinct captures are still the limit;
clones share a lease. Retained logical bytes use the existing fully decoded
record-body size (including float16-to-f32 expansion), not compressed source
size, and are released with the last lease. This is not a total-memory/RSS
cap: owned source frames, row directories/cells, retained derived indexes,
decoded records and transient authenticated plaintext remain distinct
allocations. Full-read materialization is possible; no compact-memory claim
is made. Encrypted cell materialization authenticates/decrypts the retained
ciphertext, and failed authentication publishes no cell or cache hit.

Late complete-image/truncated/corrupt-frame failures publish no Database
and leave existing handles/files unchanged. Deliberate fail-first mutations
cover the new refusals, complete-image validation, installation (including
empty-collection dispatch), snapshot pin/accounting, authentication and
exact expiry. No public API/error, wire/format/manifest, activation, policy
or default changes are introduced. Ordered public reads, scans, indexes,
lifecycle maintenance qualification and mapped installation remain later
slices.

P3c needs ID/revision lookup, vectors, visibility, metadata predicates and
generation identity to build/bind ANN codes and rerank without `RecordMap`.
P3d needs canonical stored precision plus stable vector access and fallback
behavior. The precision API needs stored precision and explicit conversion
entry points, not a new implicit read policy. None may bypass overlay,
expiry, authentication or error semantics. Do not expose this seam publicly
or let derived indexes become source-of-truth.

The extra crate-private operations P3c needs (physical base rows, source
identity, an affected-ID cursor and batched vector scoring) are proposed in
[P3c section G](p3c-compact-ann.md#g-integration-seams-and-consumers). The owner
approved the [seam freeze](p3b-seam-freeze.md#5-additions-for-p3c) shapes as
additive on 2026-11-05; they give the operations concrete signatures and map
today's `CompactView`, `CollectionRecords` and `ReadWorkspace` onto the section
G names. The physical-base and affected-ID operations are implemented (0.7.1, R0
slice 3: [section 3.7 of the freeze](p3b-seam-freeze.md#37-physical-base-affected-ids-and-ordered-ids));
the affected-ID cursor takes no `S`, so that gap is only partly closed: the floor and
lineage check waits for the binding. The portable binding and the batched vector walk
remain assumptions that stay with the ANN lane and are not part of this contract until
each is implemented.

### J. Ordered code sub-slices and shared-file touch map

Every sub-slice requires a deliberately failing regression mutation (for
example, drop the generation pin, let an overlay tombstone fall through,
swap expiry comparison, omit block authentication, or omit one rewrite
source), then restore the mutation and show the named test green. Each adds
golden/malformed coverage where it owns decoding. Sizes are planning ranges,
not performance/size claims.

| Order / sub-slice | Goal, exact files/functions, tests and acceptance | Size / dependency / parallelism |
|---|---|---|
| 0. Pinned source and standalone point-read view | **Implemented, independently accepted and integrated (private, test-gated).** Retain a verified-block directory from the same `ImageCheck` pass in an owned frame buffer; mapped backing is exercised by parity tests only. Add generation-owned once-cells and a standalone view with point reads only. Files: `src/compact/source.rs` and tests; `src/compact/mod.rs` (`ImageCheck::block`); `src/format.rs` (`load_inner`, `verified_frame`). Tests: `compact_generation_authenticates_before_publish`, `compact_generation_pins_source_until_last_handle_drop`, `compact_view_get_matches_resident_after_source_drop`, `compact_get_decodes_each_record_once`, encrypted wrong-key/tag rejection, mapped/owned parity and gate-off refusal. Accept complete-image publication, exact fields/revisions/expiry, authentication-before-publish and byte-source parity. | ~500-800 LOC; no Database/Chain/collection wiring; foundation for 1. |
| 1a. Enum and fail-closed dispatch | **Implemented, independently accepted and integrated (private, unreleased).** Migrate record consumers through exhaustive resident/compact dispatch and typed resident-only access; guard mutation acquisition and unsupported operations before early returns or side effects. Dispatch fixtures exercise point parity, refusals and source/snapshot retention; normal loader installation remains resident. No loader, directory or P4 codec hunk. | Bounded split from 1; depends on accepted 0; stop for independent acceptance before 1b. |
| 1b. Database installation and point-read acceptance | **Implemented, independently accepted and integrated (private, unreleased).** Owned verified generation installation after full-image validation; directory/segment refusal. Real-open A-D method matrix, late-image failure atomicity, explicit expiry, encrypted reads and snapshot/accounting lifetime. Slice 2 now covers the post-image WAL suffix path (tracked separately in row 2). | Depends on independently accepted 1a; minimal loader hooks only. No partial or mapped installation. |
| 2. WAL overlay and committed mutation semantics | **Implemented, independently accepted with notes and integrated (private, test-gated, unreleased).** Reconcile committed put/batch/delete/metadata/TTL suffixes over the immutable compact base, retaining tombstones and WAL durability-before-apply, CAS/revision, graph-attachment and COW snapshot behavior. Files: `src/records.rs`, `src/collection.rs`, `src/format.rs` (image-floor row capture), `src/lib.rs`, and `src/vectors.rs` (retain later-slice refusals). Tests: `compact_overlay_ordered_batch_matches_replay`, `compact_delete_tombstone_shadows_base`, `compact_expired_overlay_never_reveals_base`, `compact_metadata_update_preserves_vector_payload`, `compact_overlay_snapshot_and_revision_condition_are_immutable`, `compact_graph_attachment_tracks_effective_row`, and `compact_install_real_committed_suffix_replays_into_overlay`. No public surface, production activation or later-slice capability. | Depends on 1a/1b; selected row only; serialize with 1 and 5. |
| 3. Ordered public reads and exact/filter paths | **Implemented, independently accepted with notes and integrated (private, test-gated, unreleased).** Browse and exact/filtered search over base plus overlay match the resident path at the same clock (order, distance bits, scan statistics for a filter that no posting or block summary serves, tombstones, expiry at the clock boundary and the poisoned-handle text-only query). Hybrid, query-language ordering, grouping and facets refused compact records at this slice and moved to slice 4, where lane A (0.7.1) serves them. Scans materialized record cells at this slice: the read seam of slice 4 (lane B, integrated) provides the bounded 4 MiB vector workspace of K.3, and re-pointing the exact scan onto it is the E4 follow-up (lane A's first draft, `c859d2d`, is cherry-picked unchanged in A4 and decodes through the workspace without creating cells, but browse still materializes them). Merge base/overlay IDs, unify TTL/filter/clock semantics and bounded vector access. Files: `src/records.rs` (`browse_at`, `search_with_stats_at`), `src/collection.rs` (browse/group/exact), `src/lib.rs` (`search_in_at`, `exact_search*`), `src/hybrid.rs`, `src/query_language.rs`. Tests: `compact_ordered_merge_matches_resident_with_gaps`, `compact_exact_scan_is_bit_identical_at_same_clock`, `compact_search_filters_after_overlay_resolution`, `compact_browse_cursor_skips_tombstones_and_expiry`. Accept exact order/stat semantics and bounded scratch. Also add the slice 2 acceptance note: one poisoned-handle execute test for a text-only query (mutation QL1). | ~300-500 LOC; depends 1-2; can split API/search tests after interface freeze, but shared `records.rs`/`lib.rs` edits serialize. |
| 4. Record consumers and derived indexes | **Partly integrated, three lanes (status at v0.7.0, reconciled for 0.7.1):** A record consumers (integrated in 0.7.1 by A4 and test-gated: lane A's four patches cherry-picked, the key-value group rebuilt on K71's bounded visitor, its 47 recorded mutants replayed and 170 added, 217 killed, see [the ledgers](p3b-survivor-ledgers.md#lane-a) and [lane A4](p3b-record-consumers.md)); the slice 3 remainder, the bounded exact scan, stays a draft (`c859d2d`) that E4 owns; B derived indexes plus the crate-private read seam in `src/compact/source.rs` (integrated and test-gated: opaque generation identity, bounded authenticated vector, metadata and payload reads, source/cell/scratch accounting; its fix-forward commit `250a98b` is integrated in 0.7.1 and its 18 replay obligations are replayed, the seven that mutate the seam in R0's ledger and the other eleven in B4's, with five more survivors of lane B's report (one accepted as equivalent) and three gaps that B4 found in its consumer tests, see [the ledgers](p3b-survivor-ledgers.md#lane-b); of the P3c section G operations, the physical-base and affected-ID cursors are implemented (0.7.1, R0 slice 3; the affected-ID cursor takes no `S`, so its gap is only partly closed and the floor check waits for the binding) and the rest stay later with the ANN lane, as the [seam freeze](p3b-seam-freeze.md) proposes; coordinator-authorized because the generation, view and cell fields are private to that module); HNSW, rebuild, poll, `search_ready`, snapshots, disk ANN and GPU stay refused on compact and move to P3c (2026-11-04 ruling: the resident hot loop is not rewritten; lane B delivers the seam, an effective-row accessor and the metadata, text and rerank consumers); C server and MCP consumers (not started; its Gap 8 step 2b prerequisite is integrated). A follow-up commit (E4), now that the seam and lane A are integrated (the shared `records.rs`/`lib.rs` hunks serialize), finishes the slice 3 exact scan on the K.3 workspace: lane A's draft `c859d2d`, cherry-picked unchanged by A4, already decodes vectors and metadata through the bounded read seam without creating cells, but it lists every effective ID, decodes the metadata of every visible row and ignores block summaries. `compact_rebuild_snapshot_survives_checkpoint` moves to slice 5 (compact checkpoint stays refused until then); slice 4 proves that the derived indexes read the pinned overlay (`compact_metadata_text_indexes_verify_overlay`) and that the HNSW, rebuild, poll, snapshot, disk and GPU paths refuse compact records with unchanged state (`compact_ann_paths_remain_refused_with_unchanged_state`), and the rebuild-pin and snapshot-generation proofs move to P3c (ratified for 0.7.1, OPEN-71-04). Port payload APIs, named vectors, summary indexes, HNSW/rebuild/sidecars, metrics and record serializers. Files: `src/vectors.rs`, `src/kv.rs`, `src/server.rs`, `src/server/grpc.rs`, `src/server/features.rs`, `src/server/webhooks.rs`, `src/bin/vlite-mcp.rs`, `src/rerank.rs`, `src/property_graph/retrieve.rs`, `src/server/observability.rs`, `src/server/tenants.rs`, `src/hnsw.rs`, `src/rebuild.rs`, `src/metadata_index.rs`, `src/text_index.rs`, `src/ann_snapshot.rs`, `src/disk_ann.rs`, `src/gpu.rs`, `src/quality.rs`, `src/planner.rs`, `src/compact/stats.rs`. Tests: `compact_kv_point_and_prefix_match_resident`, `compact_named_vector_visibility_matches_resident`, `compact_metadata_text_indexes_verify_overlay`, `compact_ann_paths_remain_refused_with_unchanged_state` and, with lane C, `compact_server_materializes_only_results`. Moved to P3c and required before any compact ANN activation, so that no gate disappears: `compact_hnsw_rerank_uses_pinned_generation`, `compact_rebuild_snapshot_survives_drop_and_refused_checkpoint` and `compact_ann_snapshot_rejects_other_generation` ([P3c section H](p3c-compact-ann.md#h-test-plan-future-implementation-not-run-here)). Accept no stale index result and accurate cache/source accounting. | ~600-1,000 LOC; depends 1-3; file-disjoint vector/KV/server and index groups can run in parallel after seam freeze; `hnsw.rs`/`lib.rs` hooks serialize. |
| 5. Checkpoint, maintenance, feed and replica lifecycle | Stream complete output, pin/retire images and preserve byte-copy/copy-out/error behavior. Files: `src/format.rs` (`write_v5_image`, `BlockRun`, `apply_supplement`, `verified_frame`), `src/compact/mod.rs` (`ImageCheck::{frame,block}`), `src/directory.rs` (`build_image`, `Chain::{install,open}`), `src/lib.rs` (`start_checkpoint`, `finish_checkpoint`, `replica_extend`, feed/export hooks), `src/maintenance.rs` (`backup_inner`, `restore_inner`, `compact_inner`, `write_v5`, `write_v5_image`, `convert_precision`), `src/changes.rs` (`scan`), `src/logops.rs` (`restore_to`, `reencrypt`), `src/property_graph/{frame.rs,maintain.rs}`. Tests: `compact_overlay_checkpoint_reopen_is_identical`, `compact_checkpoint_keeps_old_snapshot_mapped`, `compact_rebuild_snapshot_survives_checkpoint` (moved here from slice 4), `compact_backup_restore_reencrypt_authenticate_all_blocks`, `compact_change_feed_starts_after_image_floor`, `compact_replica_failure_keeps_old_generation`, `compact_graph_copyout_keeps_attachment_rules`. Accept full lifecycle matrix before any writer is production-enabled. | ~700-1,100 LOC; depends 1-4; serialize with P4 checkpoint/read work. |

**Shared-file touch map for P4 checkpoint/read integration.** P4 independently
touches `src/compact/mod.rs`, `src/format.rs`, `src/maintenance.rs` and
`src/lib.rs` hooks. Before coding, reserve ownership at function granularity:

| Shared file | P3b functions/regions | P4 functions/regions | Coordination |
|---|---|---|---|
| `src/compact/mod.rs` | `ImageCheck::block` P3 kind check/apply and source block ordering (1,5). | `ImageCheck::block` graph-kind dispatch; `ImageCheck::frame` graph snapshot ordering. | Shared match arms: one owner edits at a time; separate owner-specific validators, then one serialized dispatch integration. |
| `src/format.rs` | `load_inner`, `BlockRun::{new,apply}`, `apply_supplement`, `LoadedCollection` put/delete/update, record output from `write_v5_image` (1,2,5). | `load_graph_image`, `apply_supplement`, graph snapshot/revision replay, graph section in `write_v5_image`. | Share loader and supplement state; agree on struct fields/order first and serialize integration. |
| `src/maintenance.rs` | `backup_inner`, `restore_inner`, `compact_inner`, `convert_precision`, `write_v5`, `write_v5_image` record source loops (5). | Graph-copy `enable_graph`/`strip_graph` and graph snapshot encode/revisions through `property_graph::maintain` and `write_v5_image`. | Build one common effective-record iterator; do not let either owner independently copy out `RecordMap`. |
| `src/lib.rs` | `open_backed`, `apply`, `start_checkpoint`, `finish_checkpoint`, read/search hooks and `replica_extend` (1-5). | Graph replay/checkpoint clock, graph mutation and `GraphRef` hooks. | Add distinct generation/graph fields under one coordinator; serialize `Database` field/constructor changes and checkpoint swap. |

`src/changes.rs::scan` is also shared with temporal graph feed work, although
not named in the P4 four-file touch set; reserve its `ImageCheck` and event
dispatch branches. P3b should not change graph framing or add graph codecs.

### K. Owner decisions

The owner explicitly approved each choice separately on 2026-11-02:

1. **First write:** immutable compact base plus a committed WAL overlay,
   not full resident conversion on first write. Checkpoints fold committed
   edits into a new base; the WAL remains the durability authority.
2. **Borrowed records:** non-evicting generation cells with explicit
   accounting and unchanged public APIs and snapshot caps. Reading every
   record may materialize the full collection; compact RAM is not promised
   for that access pattern.
3. **Scan scratch:** serial exact compact scans initially, with 4 MiB of
   vector-decoding workspace per operation. This is not a total-memory cap:
   cells, overlays, plaintext buffers and concurrent operations are accounted
   separately. Parallel scans require aggregate-memory qualification.
4. **Snapshot metric:** include decoded cache/overlay and precision expansion
   in logical retention, report mapped bytes separately, and preserve
   existing public counter meanings and rejection limits.
5. **Maintenance:** require an operation-by-operation parity table covering
   existing revision, lineage, import, TTL, graph and feed semantics.
   Unsupported paths stay refused until qualified; failed publication
   preserves the source and existing data-loss confirmations remain.
6. **Admission:** retain current gates/refusal. Compact storage, float16 and
   temporal copy-out require separate qualification and explicit opt-in;
   defaults and the final release go/no-go remain unchanged.

These are design approvals, not implementation acceptance, activation,
performance qualification or permission to tag a release. A guarded-view
alternative to choice 2 would require a separately approved public API change.

### L. Risks and unverified items

- **Reconciled at v0.7.0:** the setup-head absences this bullet listed are
  resolved only under `cfg(test)`. A generation owner (`CompactGeneration`), a
  compact `ReadSnapshot`, the base-plus-overlay merge, the effective-row
  accessors and the bounded read workspace exist and are accepted, but no
  release build compiles them and no compact writer is enabled. Still
  unverified: whether the production image directory can retain a single block
  without retaining a larger map, because the loader still builds the resident
  `RecordMap` first and installs the generation over it.
- **Unverified:** the 4 MiB scratch budget is a conservative choice (room for
  two of the largest decoded blocks, each at most 2 MiB), not a derived maximum;
  aggregate memory accounting with concurrent
  non-search reads, GPU staging and rebuild workers needs implementation
  measurements; no result is claimed here.
- **Unverified:** current public maintenance APIs' exact numeric revision
  behavior across every compact/precision/restore combination needs an
  operation-by-operation oracle before a production writer is allowed.
- The [storage goals](../performance/storage-goals.md) make exactness and
  S2 explicit constraints; this proposal supplies no qualification or
  performance/size evidence (Goals and Must not regress).
