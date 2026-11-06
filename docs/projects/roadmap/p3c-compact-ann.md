# P3c: persisted compact ANN design

**Design reviewed; owner decisions approved 2026-11-02; not implemented,
qualified or active.**
Inventory head: `2b045b63ac598b0cce8e27f061b6a87f3b9b455f`.
All code anchors below were checked at that head, not copied from the older
P3b inventory. This document changes no API, code, default, gate or release
criterion. The owner explicitly approved J.1-J.9 individually on 2026-11-02,
subject to the independent acceptance and dependencies below. P3b slices 0, 1a,
1b, 2 and 3 are independently accepted and integrated behind test gates, and
lane B's read seam is integrated with its fix-forward and the replayed mutation
ledgers (all 18 of its replay obligations: seven in R0's ledger, eleven in B4's; carried on the integration tree for 0.7.2); that does
not satisfy the remaining engine-integration, source-interface or lifecycle
prerequisites. The [seam freeze](p3b-seam-freeze.md) (written during 0.7.1 and carried to 0.7.2) records what the
code at v0.7.0 provides and states the section G additions as concrete shapes;
the paragraphs below keep their setup-head wording unless marked reconciled.

The governing documents are [P3](p3-compact-vectors.md),
the [storage contract](compact-storage-contract.md), the
[precision API](p3-precision-api.md), [P5](p5-filters-and-serving.md) and
the [next-major roadmap](next-major.md). Earlier documents' descriptions of
unimplemented work do not establish that work's presence at this head.

## A. Scope, non-goals and gates

Implement a crate-private, source-bound ANN representation: flattened HNSW
topology, persisted scalar-u8 codes, background rebuild/publication and a
separately opted-in rotated one-bit prototype. Retain squared-L2 and cosine;
inner-product ANN remains `UnsupportedAnnMetric`. Exact search is unchanged.
Source float32 is the default; source float16 is independently opted in and
canonicalized before commit. Neither quantization changes stored vectors.

No P3d CPU dispatch, GPU shader/staging change, named-vector storage codec,
public `Error` variant, public method/type/field, dependency, REST/gRPC/MCP/
CLI/binding field, OpenAPI or proto change is proposed. P3b owns source access,
overlay and file lifetime; P3c consumes them rather than creating another
storage layer. Named-vector ANN adapters must use the same read seam, but v1
persisted ANN below covers **primary dense vectors only**. Sparse vectors,
named model histories, graph adjacency and temporal segments are not encoded
as ANN data. P3 is not complete until its separately owned named-dense work
is done.

Keep the storage contract's crate-private prequalification gate. Add a
separate test-only P3c guard and a further explicit one-bit selection; a file
containing codes cannot enable either guard. Existing ordinary constructors,
`Quantization::None`, `ScalarU8`, `Record`, `AnnOptions` and public readiness
identity keep their meanings. No query builds compact ANN synchronously.
Ready-only calls still report `IndexNotReady`; existing exact/auto fallback
semantics remain. Internal one-bit tests use a private entry point, not an
implicit reinterpretation of a public `AnnOptions`.

No performance, recall or total-size result is claimed. For 768 dimensions
the sign payload alone is 96 B; parameters, correction terms and topology
are additional. Measurement plans below do not authorize a benchmark campaign;
DO-005 remains a major-release-only gate.

## B. Call-site inventory at the setup head

Rows group all engine call-site families, not every wrapper invocation.
`RecordMap` is `Arc<BTreeMap<u64, Arc<Record>>>`; cloning shares bodies, but
every body owns its vector (`src/records.rs:17-120`). This is not a compact
source view. Proposed replacement names are not claims of existing code.

| Verified locations | Ownership and behavior today | Required compact change |
|---|---|---|
| `src/hnsw.rs:26-36,276-340,358-370` | `Vectors` adapts `RecordMap`; `Node` owns `Vec<Vec<usize>>` links, ID and norm; `Index` owns nodes/codes/buffer; export clones links and refuses quantized graphs. `QuantizedSnapshot::encode` trains and copies codes from records. | Concrete flat topology view and bounded vector access; streamed training/encoding, no all-record table or expanded per-node links on load. |
| `src/hnsw.rs:415-549,595-641,771-850,852-941` | Traversal reads code slices or record vectors; construction builds node-local vectors of links in deterministic batches; unquantized `track` retains original vectors, quantized `track` retains changed-ID accounting. Levels are seeded, capped at 16; degree is `2*m` at level 0 and `m` above. | Reuse construction/selection/level rules through checked flat row access; construction has a separate bounded arena, not a second retained record set. Unquantized compact traversal reads pinned base vectors, not updated overlay vectors as bridges. |
| `src/hnsw.rs:943-1024,1027-1251,1259-1410` | Eligibility uses records, expiry and metadata; nonmatching nodes can bridge. P5 bounded bitmap traversal is test-only. Exact rerank and buffer merge gather `&[f32]` from records and call `Scorer`. | Visibility and predicates use one `ReadAccess`; score source codes for bridges, exclude shadowed base nodes from eligible heap, exact-score current candidates in bounded batches. Preserve P5 work accounting and completion status. |
| `src/hnsw.rs:1427-1720` | Snapshot encoder/parser owns nested links, codes and mutation-buffer originals. Checks bounds, levels, entrypoint, completeness, duplicate/self/wrong-level links, reconstructed codes and norm bits against `RecordMap`. | Keep legacy sidecar v1; reuse invariants in compact check/bind, not its byte format. Compact validation streams source rows and checks flat global ordinals; never expands a whole map to validate. |
| `src/quantize.rs:17-82,87-225` | Public two-choice enum/options; four-times rerank default; scalar min/scale in f64, rounded/clamped codes, decoded norms, asymmetric distances; snapshot parameters persisted only in sidecar. | Reuse scalar arithmetic exactly. Add private profile/rotation functions, not a public enum variant. Persist versioned parameters/norms in image blocks. |
| `src/rebuild.rs:119-172,184-272,289-368` | One worker shares full `RecordMap` or captures scalar codes; background encoding runs on worker. Job tracks changed IDs/vector sequence and publishes catch-up; limits count records/vector bytes and retained bodies. | Pin source generation and immutable overlay root; stream rows on worker, preflight arena/code/cache bytes, keep sequence-complete delta, publish only after source compatibility checks. Keep existing limits and errors. |
| `src/lib.rs:2751-2829,2857-2995,3003-3152` | Set/maintain background policy, build inline, capture/poll/cancel job, pressure/status reporting; primary index replacement owns retirement. | Compact-only scheduling uses existing worker/status machinery, never inline builds; resident behavior remains. Bounded queue and source-aware admission precede allocation. |
| `src/lib.rs:2209-2218,2514-2544`; `src/collection.rs:228-285` | Committed mutations track old records in index/job; vector sequence updates; metadata-only writes clear named indexes and update revision/postings. | After durable commit, shadow all affected IDs including metadata/TTL-only changes. Read old source vectors only via pins; delta coverage is broader than today's vector sequence. |
| `src/lib.rs:3214-3278,3369-3501`; `src/quality.rs:242-360,373-465`; `src/collection.rs:509-576` | Lazy and ready ANN entry points, validation and wrappers; lazy resident path can build inline; ready paths compare config/metric/quantization. | Dispatch concrete resident/compact handles. Return fallible read/auth errors; retain current public validation/readiness contracts. Private one-bit path has its own profile identity. |
| `src/ann_snapshot.rs:252-386,388-459,469-554`; `src/lib.rs:3648-3688` | `.ann` identity includes version/lineage/sequence/end; digest streams ID/revision/f32 vectors. Two-pass checksum/parse, all-or-none install, staged fsync/rename. Save refuses encryption; load does not have a matching cipher check. | Do not transplant sidecar identity/format into blocks. Keep explicit plaintext sidecars legacy; compact generations use in-image codec and full binding. Compact encrypted paths never invoke either sidecar reader or writer. |
| `src/disk_ann.rs:145-182,186-199,201-301` | Build refuses encrypted DB, builds ordinary in-memory HNSW and writes plaintext vectors/links into a staged `.dann`; any database write makes its sequence identity stale. | Preserve sidecar API and encryption refusal, not an escape path for compact ANN. Accessor-port plaintext build separately; it must not materialize records. No compact-block export through `GraphExport`. |
| `src/disk_ann.rs:305-404,435-553,568-665` | Header/upper layers resident, per-node CRC read, mutex-protected logical-LRU node cache with owned f32 vectors; search has no metadata/TTL filtering. `verify` streams nodes. | Reuse cache discipline, not plaintext I/O/layout. Compact handle has generation-keyed authenticated blocks and effective visibility; do not broaden standalone DiskANN semantics. |
| `src/vectors.rs:383-419,451-497,702-719`; `src/collection.rs:134,162,230,257` | Named ANN makes a temporary `RecordMap` via `named_records`, builds an unquantized index in `(name,version)` cache; mutations clear it. | P3b source adapter must avoid cloned primary records and named vector maps for scoring. Keep named indexes volatile in P3c v1; no selector ambiguity with primary blocks. |
| `src/records.rs:164-225,253-261,595-635`; `src/collection.rs:159-181,202-225` | Snapshots share resident records/expiry, exact searches and derived metadata/text rebuilds consume them; loaded collection starts with no ANN index. | P3b pins source/overlay; compact ANN installation occurs only after complete image validation and binding. Exact fallback streams effective vectors, not a legacy iterator. |
| `src/metadata_index.rs:316-343,348-425`; `src/text_index.rs:96-169` | Metadata postings/summary verification and text index rebuild/track consume records/metadata; postings are derived. | P3b ports rebuilds; P3c rechecks effective metadata/expiry. No coupling of ANN ordinal space to posting order. |
| `src/planner.rs:77-181`; `src/hybrid.rs:155-203` | Auto chooses exact/indexed/ready ANN using physical count; hybrid dispatches vector leg, filters text hits against current records/expiry. | Same generation/clock across legs and final predicate checks; no new planner policy/default. ANN cannot prove strict top-k. |
| `src/quality.rs:91-237`; `src/hnsw.rs:380-393` | Readiness, graph/link/code/buffer counters; resident estimate `count*dimensions*4`; quality samples stored vectors against exact. | Preserve public reports; internally separate resident/cache/source/arena bytes, sample via bounded accessor. No fake recall on unmeasured one-bit mode. |
| `src/exact_kernels.rs:96-219,223-252`; `src/gpu.rs:36-44,96-149` | Batched portable/dispatched exact scorer has ordered f64 accumulation; GPU gathers all live record slices, stages chunks and CPU-rescores selected rows. | Feed P3d exact scorer bounded canonical f32 slices. GPU adaptation belongs to P3b, not a new P3c GPU path or shader. |
| `src/property_graph/retrieve.rs:370-398,464-484` | Hybrid vector pivot, record predicate recheck, attachment mapping, record metadata/payload evidence. | Use same pinned effective read for pivots and evidence; graph time selector/TTL remains independently authoritative. No graph view from ANN links. |
| `src/compact/mod.rs:33-62,125-144,383-469,707-865,1189-1320,1515-1641,1664-1723` | Private/test gates; 64 MiB block cap; bounded skip summary; prefix/envelope/table/end checks. All authentic declared derived kinds are skipped, even `0x0400`; no ANN codec dispatch. | Storage owner registers concrete check/apply/encode/bind and bounded outcomes, preserves eager validation and existing refusal gates. |
| `src/compact/vectors.rs:35-42,71-86,101-167,221-353`; `src/compact/half.rs:21-72` | Test-only primary dense rows, max 4,096 rows/1 MiB components, checked layout and finite canonical stored precision. | Reuse checked source layout/half conversion; no second primary format or re-rounding for rerank. |
| `src/compact/catalog.rs:21-84`; `src/compact/catalog/column.rs:13-157,179-345` | Precision codes 1/2; test-only column accessor/directory/cache. Even f32 is decoded into scratch until pinned access exists. Cache keyed by caller block number, not global generation. | Bind checked generation key externally, preserve shape guard and budget; never treat caller-supplied block number as cross-generation identity. |
| `src/format.rs:348-418,946-1048,1835-2085,2410-2499,2503-2603,2654-2674` | Loaded records resident; settings blob v1 rejects trailing/version bytes; load/check/supplements and legacy block runs decode records, footer/auth precedes use. | Storage owner adds compact derived installation after end/source checks, no settings-v1 extension. P3c private settings live in descriptor below. |
| `src/lib.rs:1492-1535,1754-1825`; `src/maintenance.rs:371-453,493-542,901-1118` | Open constructs collection state; checkpoint captures/writes/installs loaded image; byte copies versus rewrites have distinct identity/TTL behavior. | Storage owner optionally carries a compatible complete ANN set or pending descriptor; never waits for rebuild. Omit incompatible sets before table construction. |

Serving/CLI wrappers do not own another ANN engine: representative callers
are `src/server.rs:2843,3359`, `src/server/ops.rs:431`,
`src/server/admin.rs:886`, `src/bin/vlite-mcp.rs:643` and
`src/bin/vlite-shell.rs:194`. Server sidecar lifecycle is
`src/bin/vlite-server.rs:600-629,677-680,800`; standalone DiskANN loading
also appears at `src/server/grpc.rs:1925` and
`src/server/features.rs:686`. These retain existing contracts; no serving
schema changes are required to use scalar compact ANN. Bench/test callers
are not production index owners and no benchmark is run for this design.

## C. Persisted derived codec v1

All layouts here are proposals, **not existing bytes**. Allocate `0x0200`
descriptor/settings, `0x0201` topology, `0x0202` scalar-u8 rows and `0x0203`
rotated-one-bit rows. Leave `0x0204-0x02FF` reserved. Every block has codec
version 1 and flags **0** (non-critical); no new capability/header bit or WAL
op. Use op 16 only in the checkpoint restatement at sequence S, after source
blocks and before P5 summaries, declared in the existing capability table.
The image end includes these frames as usual.

Use the contract's 32-byte envelope unchanged: kind/version/flags/section
count at bytes 0-7, object at 8-15, source digest at **16-23**, first section
offset at 24-27, reserved zero at 28-31. Object is
`collection_id | (partition << 16)`; prefix collection, kind and partition
must agree; high 16 bits zero. Section table is `(offset u32,length u32)`.
All integers and IEEE float bit patterns are little-endian. No native
`usize`, pointers, absolute disk offsets or native-endian casts.

Sections start at the next 64-byte-aligned file position with the minimum
0-63 zero padding, including before section 0; last section ends the body.
Readers accept contract-legal advisory alignment after re-encryption and
copy misaligned/encrypted bytes into bounded scratch. Deterministic bodies
mean identical logical image inputs **and identical frame file positions**;
section payloads and index digests are independent of padding/position.
Re-encryption keeps logical bytes but need not keep ciphertext or alignment.

### C.1 Descriptor (`0x0200`, partition 0, count 1)

Exactly three sections: binding (224 bytes), private settings (32 bytes),
parameters (profile-specific below). Only one index set per collection;
primary selector is implicit. Binding fields, in concatenation order:

| Offset | Field |
|---:|---|
| 0 | magic: eight ASCII bytes `VLCANN01` |
| 8 | binding version u16 = 1; collection u16; dimensions u32 |
| 16 | precision u8 (1 f32, 2 f16); metric u8 (0 squared-L2, 1 cosine); profile u8 (0 none, 1 scalar, 2 rotated); state u8 (0 requested/no index, 1 complete); reserved u32 = 0 |
| 24 | source lineage u64; checkpoint sequence S u64 |
| 40 | generation identity SHA-256 (32 bytes) |
| 72 | bound source-block digest SHA-256 (32 bytes) |
| 104 | logical primary content digest SHA-256 (32 bytes) |
| 136 | effective source physical row count u64; graph node count N u32; topology partitions P u32 |
| 152 | code partitions Q u32; entry ordinal u32 (`0xffffffff` if N=0); m u16; ef_construction u16; max level u8; quantizer version u8 = 1; build version u16 = 1 |
| 168 | HNSW seed u64; quantizer/rotation seed u64 |
| 184 | index-set digest SHA-256 (32 bytes) |
| 216 | reserved eight zero bytes |

State 0 requires N=P=Q=0, entry sentinel, max level 0, index-set digest all
zero and no topology/code blocks. Source physical count may be nonzero.
State 1 permits N=0 (all cosine rows zero or empty source); P=Q=0 then,
entry sentinel and max level 0. Nonempty complete sets require both topology
and selected codes (Q=0 for profile 0), full coverage and one valid entry.
Parameters and settings still exist for empty/pending descriptors.

The index-set digest is SHA-256 of ASCII `VLCANN-SET-1` followed by settings,
parameters and every topology/code block in ascending `(kind,partition)`,
each as `kind u16, version u16, partition u32, count u32, section_count u16`,
then for each section `length u32, payload bytes`. Envelope/padding are
excluded. Descriptor/binding are excluded to avoid circular hashing. Source
binding and descriptor fields have their own checks. No digest is keyed;
confidentiality/authenticity comes from the existing frame/AEAD boundary.

### C.2 Topology (`0x0201`)

Five sections, in this order:

1. Header (32 B): global start ordinal u32, row count r u32, local layer-list
   count L u32, local neighbor count E u32, global N u32, reserved 12 zero B.
2. Node rows, r times 24 B: public ID u64, base revision u64, source row
   ordinal u32, level count u8, three reserved zero B.
3. Node-to-layer offsets: r+1 u32 values, starting 0, ending L. Difference
   equals that node's level count, in 1..=17.
4. Layer-to-neighbor offsets: L+1 u32 values, starting 0, ending E.
5. Neighbors: E global node ordinals u32. Lists are node-major, level
   ascending; each neighbor list is sorted by global ordinal.

Global node ordinals are assigned by ascending public ID across partitions;
source-row ordinals address physical **base** rows, never effective-overlay
positions. Check source ID/revision agreement and one-to-one eligibility
coverage. Cosine zero-norm source rows are excluded; expiry does not remove a
row retained by the source image, but always masks results at query time.
No self edges, duplicates, out-of-range ordinals or neighbors absent from
that level. Degree <=2*m at level 0 and <=m above. Recompute the existing
`Index::level(id)` rule with the bound HNSW seed, including its level-16 cap.
Entry is the first ascending-ID node at the maximum level, as current
snapshot validation requires; descriptor max level must equal that maximum.
Empty lists are legal; disconnected graphs
are not claimed to have exhaustive recall.

Sorting lists makes serialization canonical; use the same canonical order
for compact build searches **before** publication, not just on save. Do not
promise identical candidate sets to an old nested-link graph when equal
distances and traversal ordering differ. Exact output distances/ties remain
the oracle; immediate-versus-reopen compact behavior must match.

### C.3 Scalar rows (`0x0202`)

Three sections: 16-byte header `(start ordinal u32,r u32,dimensions u32,
reserved u32=0)`; r*dimensions node-major bytes; r f64 decoded Euclidean
norms. Norm is positive/zero as computed by `ScalarU8::norm`, even for L2
(L2 traversal ignores it). Check finite nonnegative norm bits by recomputing
from codes. Descriptor parameters are `dimensions u32,reserved u32=0`,
then dimensions f64 minima followed by dimensions f64 scales. Finite
minima, finite nonnegative scales, constant dimensions scale +0 and code 0.
Empty training yields all +0 parameters; the dimensions count remains the
collection's dimensions, not an empty 0-dimension exception. Persisted
minima are canonical: `f64::min` does not specify which zero a tie returns,
so a -0.0 minimum is encoded, and compared at bind, as +0.0, and a stored
-0.0 minimum is malformed. Codes and scales are unchanged because +0 and -0
both quantize to code 0 (ruling P3C-scalar-min-signed-zero-canonical,
provisional).

Training excludes zero vectors for cosine and otherwise includes all base
rows. Reuse existing f64 min/max, scale `(max-min)/255`, round-half-away-
from-zero and clamp 0..255; never substitute binary16 rounding. Bind
reconstructs every code from stored source values and checks parameters
against independently retrained bounds. Profile 0 has empty parameters
and no code blocks; traversal reads immutable canonical base vectors and
computes base norms through bounded source access.

### C.4 Rotated rows (`0x0203`)

Three sections: same 16-byte row header; r*ceil(dimensions/8) sign bytes;
r rows of four f64 values `(norm_squared, alpha, delta, residual_squared)`.
No per-row padding between sign codes; bit i is bit `(i%8)` in byte
`i/8`, least significant first. Unused high bits of the final byte are zero.
Norms and correction terms are additional to the sign payload.

Parameters are exactly 40 B: family u16=1, algorithm version u16=1,
rounds u16=8, reserved u16=0, dimensions u32, reserved u32=0,
rotation seed u64, c bits u64=`0x3fe3333333333333`,
s bits u64=`0x3fe999999999999a`. Algorithm is specified in F.
All rows/terms are reconstructed and bit-checked against source at bind,
not trusted because their checksum passes.

### C.5 Bounds, assembly and rejection

Proposed private format limits: dimensions 1..=65,536 (the current
`MAX_DIMENSIONS`, `src/lib.rs:95`), physical source rows and N <=1,000,000;
m 2..=64; ef_construction m..=4,096; 17 levels; at most 1,152 neighbors
per node (128+16*64). P and Q <=1,000,000, including descriptor consistency,
and must match actual contiguous runs. Every body <=**4 MiB**, a smaller
P3c bound inside the contract's **64 MiB** cap. Target body <=1 MiB.
No decoder may allocate a whole-image `N*dimensions` array from counts.

Each topology partition contains at most 256 complete nodes, never splits
a node; code partitions at most 4,096 complete rows. Greedily take the
largest prefix fitting the 1 MiB target including envelope/table and worst
63 B padding per section; allow a single row up to the 4 MiB hard cap.
If a complete row cannot fit, refuse the optional build explicitly with
existing `HnswRebuildTooLarge`; never truncate a degree or vector. Readers
accept any contract-valid partition split within the hard limits; encoders
use the canonical greedy split. Partition counts cover ordinal ranges
0..N with neither gap nor overlap; topology/code partitions need not match.
For node/code blocks prefix count equals r; r>0, no empty partition.

Before any allocation: verify prefix cap, authenticate, parse envelope;
checked-add/multiply all lengths and counts in u64, then checked-convert
to usize/u32; check against remaining section bytes, hard limits and
admission budget. Bound L <=17*r and E <=1,152*r, verify exact section
lengths and offsets before following them. Stream global cross-partition
checks with the pinned directory, bounded neighbor scratch and ID lookup;
do not reconstruct nested `Vec<Vec<_>>`. Hash input lengths are explicit
where variable; check every count even when the source digest is stale.

Unknown kind: validate frame/envelope/capability/run/end, skip/report
`unknown_kind`. Known kind with unsupported codec, binding, settings,
quantizer or build version: validate the supported outer framing, skip the
**entire collection's index set**, report `UnsupportedVersion` /
`unsupported_version`; do not guess an unknown inner layout. Supported
v1 structural failure, partial set, dangling child, missing declared
partition, invalid ordinal/term, checksum or authentication failure:
existing `Error::Corrupt { offset, reason }`, **not** a cache miss, stale
result or repairable tail. A valid but differently bound source/config is
`Stale` / `stale`, discard the whole set and schedule supported rebuild.
Unsupported layouts can only be checked at understood framing boundaries;
an unknown inner structure is not asserted valid. Corruption detected at
any understood boundary always wins over a recoverable skip.

Complete image validation is eager, before serving, on every reader path.
A foreign valid set is stale rather than code/source corruption: first
validate its standalone structure/digest, compare binding, then perform
source-dependent reconstruction only when the full binding matches.
Candidate installation is atomic per collection after the whole image
passes, never partially installs topology without matching codes.

## D. Source binding, overlay and staleness

Define the binding without depending on image-end prefixes or derived data:

- **Bound source-block digest B:** SHA-256 of ASCII `VLCANN-SOURCE-1`,
  collection u16, dimensions u32, precision u8, then primary complete-record
  source blocks in source partition order, each as `kind u16,version u16,
  partition u32,count u32,section_count u16`, followed by each section's
  `length u32,payload`. Include dense, ID/revision/expiry and complete-record
  payload/metadata sections of that primary base, not named/sparse/graph/P5
  or derived sections. Padding, envelopes, frame crypto and offsets excluded.
  V1 emits ANN only alongside a compact primary source, not legacy op-14;
  no pretend digest for a legacy `RecordMap`.
- **Envelope digest:** the first eight raw bytes of B at bytes 16-23 of
  every ANN envelope. This is a quick source-block guard, not collision
  proof; always compare full B. All children must agree with the descriptor.
- **Generation identity G:** SHA-256 of ASCII `VLCANN-GENERATION-1`,
  lineage u64, checkpoint S u64, collection u16, B (32 B). This is an ANN
  source-generation identity, not a process-local address or hash of the
  whole image. Separate runtime generation token pins the actual file.
- **Content digest C:** SHA-256 of ASCII `VLCANN-CONTENT-1`, collection u16,
  precision u8, dimensions u32, physical rows u64, then each ascending-ID
  base row as ID u64, revision u64, expiry-present u8, expiry u64 (0 when
  absent), followed by exactly dimensions stored component bit patterns
  (f32 u32 or canonical f16 u16). Preserve signed zero; do not hash rounded
  query values. C binds ANN-relevant logical content while B also binds the
  containing record sections. B/C are computed incrementally.

Check all descriptor fields against checked source metadata; build config
and seeds against requested private settings; versions against explicit
supported versions. A checkpoint that changes source partitions, TTL
retention, revisions, metadata sections, lineage or S cannot just copy an
old descriptor. No digest is accepted from a manifest filename, count or
shape alone. No circular hash includes descriptor or derived bytes.

The persisted graph covers base S, not the current WAL head. Query captures
`(base G, committed overlay root at T, clock)` once. Select the newest
committed winner by public ID; tombstone or `expiry <= clock` hides it,
including an expired newer put over a live base. Current metadata predicates
are checked after winner selection. Base topology/codes may still traverse
shadowed or expired nodes as bridges, using their **base** values. Such
nodes never enter the eligible base heap.

Enumerate the captured overlay's distinct affected IDs since S, including
new/replaced/deleted, metadata-only and TTL-only changes. For every current
live matching put, exact-score its current stored canonical vector; deletes,
expired puts and cosine zero vectors yield no hit. Exclude all these IDs
from base eligibility, even if vector bytes did not change. Union these
overlay candidates with base ANN candidates, resolve IDs/revisions again
within the same captured view, exact-rerank using current values and order
distance then ID. Each ID appears once. Reranking only reorders retrieved
base candidates plus explicitly enumerated overlay candidates; it cannot
recover an unvisited base neighbor or prove exact top-k recall.

Proposed compact ANN delta bound is the existing 256 distinct IDs and
8 MiB vector accounting, including deletes in ID count. This is an ANN
acceleration bound, **not** a source overlay/write limit. On crossing it,
invalidate compact readiness and schedule background maintenance; do not
drop deltas, reject a durable write, serve an incomplete buffer or block on
construction. Ready-only returns `IndexNotReady`; auto/existing background
query path uses exhaustive effective-source fallback and reports pressure.
Do not impose this bound on resident background mode, which currently
permits larger buffers. A worker building the exact immutable base S can
publish with <=bound deltas to T; otherwise remain unready and request a
checkpoint/new base. No repeated rebuild of S is promised to shrink its
delta. Storage overlay bounds/checkpoint pressure remain P3b decisions.

## E. Rebuild, publication and lifecycle

Use one existing `Database.rebuild` job across collections. Capture a pinned
source/overlay token without encoding on the query thread, reserve budgets
and enqueue; train/encode/build/check on worker. For persisted-base builds,
build immutable source S and retain a sequence-complete affected-ID delta
to T, not a merged T graph falsely labelled S. Check source generation,
profile/config, cancellation and delta coverage at publication. If any
changed, finish `Superseded`, `Cancelled` or `CatchUpOverflow`; never replace
a ready valid handle with a partial one. Worker panic/I/O failure uses
existing `HnswRebuildFailed`/`Io`, explicit logging and caller error.
Keep old compatible handle while work runs; expired/deleted IDs stay masked.

Publishing a ready in-memory handle is not checkpoint publication. Worker
completion does not mutate an installed immutable image or create sidecars.
A checkpoint captures effective source at S2, emits complete source first,
then either (a) a fully built set bound to those exact output source
sections or (b) **only state-0 descriptor** recording supported private
settings if the compact profile was explicitly requested. It must not
wait for an ANN job. Without that explicit request, emit no P3c blocks.
If source/output changes, (b) is normal; background builds the new source,
and a later checkpoint can carry a complete compatible set only if it can
be deterministically rebound/revalidated against its exact output.
Rebinding may reuse topology/codes when rows and eligible set are identical,
but recomputes B/G/C and checks every invariant; never just edits eight
digest bytes. TTL-pruned output requires a new graph, or omission.

Storage owner declares only emitted kinds/versions before capability table,
includes them in end-block prefix digest, validates staged bytes, syncs and
publishes through the unchanged manifest. Failure before publication leaves
old image/WAL/handle intact; preserve existing indeterminate-publication
errors after rename. No query waits for optional ANN persistence.

Existing status surfaces `hnsw_maintenance_status[_with_options]`, progress,
poll and health expose scalar/none rebuild state and pressure; do not add
fields or enum variants. One-bit selection/progress is private with bounded
`log` diagnostics until a separate surface review; absence of a public
one-bit selector is intentional, not permission to hide it behind None.
Failure is not reported as Idle success. Cancellation is cooperative at
training rows/build batches/block boundaries; poll/close joins and reports
failure using the existing machinery. The no-thread/WASM private path uses
exact source fallback and logs deferred rebuild, never inline ANN construction.

### E.1 Bounded memory and encrypted reads

Proposed additional private budgets, not changes to public limits: one
256 MiB rebuild arena reservation including flat offsets/neighbors/codes,
ID/revision tables, quantizer parameters and work queues; respect stricter
existing `HnswRebuildLimits` and retained-body caps. Preflight worst-case
space with checked arithmetic; if it cannot fit, report
`HnswRebuildTooLarge`, serve exact fallback, no spill or truncated graph.
The 1,000,000-node format maximum is not a promise the arena admits it.
Builder may use one bounded mutable flat arena with fixed degree slots;
it must not build an unbounded nested graph then flatten a duplicate.

For encrypted files propose a **16 MiB per-database shared decrypted ANN
block budget**, including pinned cache entries, with deterministic
logical-counter LRU of unpinned entries. Cache key is `(runtime generation
token, collection, kind, partition, codec version)`; shape guards do not
substitute for identity. Cache stores complete authenticated plaintext
bodies <=4 MiB, never raw decrypted file mappings or per-index vectors.
Pin entries only for a bounded scoring batch. Admission reserves bytes
before decrypt/allocation; pinning does not exempt entries from the budget.
If all space is pinned, bypass cache with at most one <=4 MiB operation-local
block, releasing before next; at most two such transient buffers per active
query/rebuild operation, charged separately. Source vector scratch remains
P3b's proposed 4 MiB per operation; one-bit rotation adds two arrays of
dimensions f64 plus one dimensions u32 permutation (<=1,310,720 B).
Traversal visited sets/heaps also need checked reservation (N-bit membership,
bounded ef/results plus frontier); ef is **not** a total memory/time bound.
Admit or explicitly fail on allocation/budget exhaustion, never return an
empty successful result. Report aggregate concurrent operations/pins; a
per-operation bound alone is not S2 qualification.

All accesses: bounded read, checksum, AEAD authentication, envelope/codec
check, binding, then exposure/cache insertion. Eager open verifies all
blocks, but need not retain them; cache rereads use immutable checked file
ownership and authenticate again. Cache eviction zeroizes plaintext where
the existing crypto ownership allows; it never promises OS page erasure.
No plaintext `.ann`, `.dann`, temporary vector file or spill for encrypted
stores. The public plaintext sidecar limitations stay; use the compact
in-image boundary, not a workaround.

Public legacy sidecar calls must not install a resident v1 sidecar into a
compact handle and bypass its binding/cache rules. For a compact-backed
source, propose `load_ann_snapshot` returns existing
`Rejected(Stale("source_representation"))` without opening a sidecar, and
`save_ann_snapshot` returns existing `SidecarUnavailable`. Keep resident
sidecar behavior, including its current load behavior, unchanged. These
are new-backing dispatch rules, not a new sidecar format or a blanket
change to legacy encryption policy. Private compact persistence is only
through the image codec.

### E.2 Retention and transport

Reader/worker pins the actual source generation and associated directory
until all slices/handles are released. Swap only after durable publication;
retired handle/cache keys never alias the new runtime token. Last pin drop
unmaps before Windows unlink. Cancelling a rebuild drops its pins only after
the worker no longer reads them. Snapshots can outlive Database unchanged;
their old overlay/source clock does not consult the newly installed index.

Byte-copy backup/pack preserves blocks and verifies all of them. Restore
that preserves source identity may reuse a valid set but gets a fresh runtime
token/cache. Lineage/sequence-changing restore, copy-out precision conversion,
graph rewrite or changed source layout rebinds/revalidates or omits ANN,
never carries a foreign binding. Conversion preserves its approved
source/retention/TTL/revision contract; P3c does not redefine it. Re-encryption
authenticates and seals each block, keeps logical source digest and profile,
uses no plaintext sidecar and tolerates shifted alignment. Reopen validates
source/base first, installs only after complete image and WAL replay
coverage; it can rebuild without exposing stale candidates.

Replicas bootstrap from checked backup; post-floor WAL changes fill the
same delta, and image replacement invalidates ready state atomically.
Failed replica append/replay cannot publish a new source/index. Changes/feed
and log export eagerly verify ANN framing/known structure/binding but never
schedule a worker or emit ANN as user changes. Existing history-floor,
capability refusal, backup/key and graph opt-in policies remain unchanged.

### E.3 Replace the slice-0 skip-everything report

Today `ImageCheck::block` unconditionally adds a skip after envelope checks.
Once this codec exists, known valid supported P3c sets are checked/bound,
installed on load and **not** counted as skipped. Preserve one post-success
warning record, target `vlite::compact`, `compact_block_skip`, existing reader
labels and at most 16 groups plus exact totals; no per-block unbounded log.
Extend each kept group's private diagnostic with `outcome`
(`unknown_kind`, `unsupported_version`, `stale`) and `rebuild`
(`queued`, `deferred`, `none`); top-level outcome is common outcome or
`mixed`, and rebuild is common value or `mixed`. Group identity remains
owner/kind/version; outcome precedence within group is unsupported version,
then stale, then unknown. Structural corruption refuses the reader and
produces no success skip record.

Queue only recognized, supported rebuild profiles from a successfully
opened writable handle, subject to unchanged admission/gates. Unknown kind
or version is not decoded to invent a rebuild profile: rebuild the collection
using recognized source `CollectionSettings` (None or ScalarU8), not guessed
one-bit parameters; log `deferred` if no supported profile/job slot exists.
Read-only/inspection/feed/export use `none`; no maintenance side effects.
State-0 known settings report a separate bounded `compact_ann_rebuild`
event, not a fictitious unsupported block. Server keeps the same warning
once at startup in its existing maintenance/admin-log path; it does not
install another logger. Unknown non-P3c kinds retain their skip semantics.
Implementing this contract-prescribed transition requires the storage
owner's shared report code; this document does not edit it.

## F. Dimension-preserving rotated one-bit prototype

This is a deliberately specified **sign reconstruction prototype**, not a
complete RaBitQ implementation and not an unbiased/error-bound claim.
Scalar-u8 and unquantized traversal remain explicit alternatives.

Define family 1 as eight rounds of seeded signed pairwise rotations on
exactly d coordinates. No Hadamard power-of-two expansion, centroid, random
floating normal generator, trigonometric function or external RNG.
All integer RNG arithmetic wraps modulo 2^64. Stream starts at the persisted
rotation seed. Each next output: add `0x9e3779b97f4a7c15` to state, then
`z=(z^(z>>30))*0xbf58476d1ce4e5b9`,
`z=(z^(z>>27))*0x94d049bb133111eb`, output `z^(z>>31)`.

For each round, initialize permutation [0..d). Fisher-Yates for i=d-1 down
to 1 uses rejection sampling of next outputs: for n=i+1, compute in u128
`limit=floor(2^64/n)*n`, discard outputs >=limit, use output%n. Then consume
one next output per coordinate in ascending original coordinate order;
low bit 1 negates that coordinate (0 leaves it). Pair consecutive entries
of the permutation. With x,y the **old** pair, write
`x'=(c*x)-(s*y)`, `y'=(s*x)+(c*y)`, with the exact c/s f64 bit patterns in
C.4. Odd last entry is unchanged after its sign flip. d=1 still performs
all rounds/sign flips. Consume no random output for pairing. Same stream
continues between rounds; no reseed. Query and every source row use this
same transform R. In real arithmetic c=3/5,s=4/5 is orthogonal; binary64
arithmetic is its explicitly approximate implementation, not exact norm
preservation. No padding dimension is introduced or stored.

Reference arithmetic: convert canonical source f32 (including expanded f16)
to f64; all separate operations round-to-nearest-even binary64, no FMA,
reassociation or fast-math. Transform pair order as above. Sum in increasing
coordinate order from +0. Terms per row:
`y=R(v)`, sign_i = +1 when y_i>=0 (both signed zeros choose +1), otherwise
-1; `alpha=sum(abs(y_i))/d`; `norm_squared=sum(f64(v_i)*f64(v_i))`;
`delta=norm_squared-((f64(d)*alpha)*alpha)`;
`residual_squared=sum((y_i-alpha*sign_i)*(y_i-alpha*sign_i))`.
All terms finite; alpha/norm/residual nonnegative; delta may be negative
from rounding. Signed-zero term bits follow the prescribed operations,
not an implementation's zero canonicalizer. Empty sources have parameters
but no row terms.

For query q, compute q2 from original f64 components in order, and R(q)
with the same parameters. `dot_sign=sum(R(q)_i*sign_i)` in order,
`dot_est=alpha*dot_sign`. Squared-L2 estimator:
`max(+0, ((q2+((f64(d)*alpha)*alpha))-(2*dot_est))+delta)`.
Cosine estimator: `1-clamp(dot_est/(sqrt(q2)*sqrt(norm_squared)),-1,1)`;
query zero retains existing invalid-cosine validation, source zero is not
in cosine topology. sqrt means correctly rounded binary64 square root.
Residual is a checked diagnostic term, **not** a safe pruning/error bound.
delta corrects the reconstruction's norm, not arbitrary lost directions.
Nonfinite arithmetic is an explicit build/query error (`InvalidQuery` for
unsupported numeric operation, `Corrupt` for inconsistent persisted terms);
never insert NaN into candidate ordering.

Implement scalar reference encode/score in `src/quantize.rs` with fixtures
independently generated by H's oracle. Construction uses exact canonical
source distances through bounded access (same HNSW algorithm), then one-bit
codes only for serving traversal; it does not decode sign vectors as if
they were exact source. Bind verifies all generated codes and correction
bits. Version 1 pins build version 1 and the scalar traversal definition;
different transforms/build arithmetic require an explicit new supported
version, not a silent dispatcher substitution.

### F.1 Versioned private settings and opt-in

The descriptor's private **settings v1** section is exactly 32 B:
version u16=1, byte length u16=32, profile u8 (0/1/2), rerank multiplier u8
(1..=16; default 4), flags u16=0, ef ceiling u32=100,000,
rotation seed u64, rotation family u16 (1 for rotated, otherwise 0),
rounds u16 (8 for rotated, otherwise 0), reserved eight zero B.
Seed is 0 for none/scalar; descriptor quantizer seed agrees. This is not
`CollectionSettings` v2 and does not append bytes to op-7 settings v1,
whose decoder rejects trailing bytes. Build config/metric remain descriptor
fields and must agree with source settings for public none/scalar use.
Settings bind all traversal/scoring parameters and survive **compact
checkpoints**, including state-0 descriptors when no graph is ready.

A private test-only `CompactAnnSettingsV1::rotated(seed)` request opts into
profile 2; no public constructor/field is added. Reopen recognizes preference
only with the same private guards. It is an **advisory derived-index
preference**, not authoritative source policy: older compact-aware readers
may skip/drop it and rebuild None/ScalarU8 without losing source values or
precision. Rewrites that intentionally omit derived data can lose this
preference; they must report the omission, never claim it persisted.
If durable user-facing selection through such rewrites is required,
J.2 requires a separately reviewed settings/API design; this proposal
does not smuggle an authoritative setting into optional blocks.

For quantized candidates use the existing width
`max(k,min(ef,k*rerank_multiplier))`, checked/saturating as today, default
multiplier four; exact-rerank every retained candidate from **current
stored values**, not sign/u8 reconstructions or original pre-half input.
Existing validation enforces ef in 1..=100,000 and ef>=min(k,physical count).
P5 may impose its tighter gated ef/work caps; do not silently widen them.
No result labels approximate traversal as exact top-k; full-value distances
only establish score correctness for selected candidates.

### F.2 Equal-recall acceptance plan, no results

At major-release qualification, compare none/scalar/rotated profiles on the
same stored precision, committed generation/clock, metric, build seeds,
queries, k, filters and exact oracle. Include low/odd dimensions, 768,
anisotropic/correlated vectors, near-zero/zero, duplicates, selective
anti-correlated filters, updated overlays and encrypted cold/warm reads.
Pre-register recall targets/datasets under unchanged release criteria;
measure candidate recall **before** rerank, final recall and predicate
validity separately. Sweep bounded ef and multipliers 1/4/16, recording
unmatched recall targets as failures, not comparing faster unequal-recall
points. Report p50/p95/p99, rebuild/reopen cost and all source/cache/code/
parameter/topology/overlay/resident bytes at matched recall, including
confidence/repetition protocol. No current result or new release threshold
is supplied; meeting a target would still not authorize default activation.

## G. Integration seams and consumers

P3b proposes `CollectionRecords` plus `ReadAccess`, not an implemented
trait. P3c needs `len` for physical counts/bounds; `contains_id` and `row_of`
for effective membership and mapping; `id_at`, `revision_at`, `expiry_at`
for binding/visibility; `vector_at` for two-pass scalar training, code
reconstruction, unquantized bridges and final exact rerank; `metadata_at`
for predicate rechecks; `ordered_visible` for exact fallback/overlay merge.
`payload_at` is **not needed to score ANN**, but graph/hybrid evidence uses
it after selecting visible IDs. No scoring operation calls the stable
borrowed-Record materializer or unbounded legacy iterator.

Gaps to freeze with P3b: its listed methods do not define (1) a physical
immutable-base cursor including expired retained rows for digest/build;
(2) runtime pin plus portable B/G/C inputs, source precision and image
floor/lineage; (3) a sequence-bounded affected-ID overlay cursor including
metadata/TTL-only changes; (4) base-only vectors for shadowed bridges;
(5) fallible bounded batched vector views usable by `Scorer`. Propose
crate-private `ReadAccess::{base_rows, base_vector_at, source_identity,
affected_since, score_batch}` with explicit lifetimes/error propagation,
not a second public trait. `base_rows` iterates physical source rows;
`affected_since(S)` must cover every commit S..T or fail explicitly.
The [seam freeze](p3b-seam-freeze.md#5-additions-for-p3c) states these as
concrete signatures, and the owner approved them as additive shapes on
2026-11-05 (OPEN-71-03). Lane R0's slice 3 implemented `base_rows`,
`affected_ids` (for `affected_since`) and `ordered_ids` (crate-private, under the
seam's test gate). `affected_ids` takes no `S` and cannot fail: it lists the whole
overlay, which is the delta since the base's own floor, so gap (3) is only partly
closed. The check that the `S` and lineage of the ANN's graph equal the base's moves
to `binding` (for
`source_identity`), which with `for_each_vector_block` (for `score_batch`) stays with the
ANN lane. `base_vector_at` is the existing `CompactView::read_vector`. Metadata
decode/auth failure remains an error, never predicate false.

P3d supplies exact kernels over canonical expanded stored values with f64
accumulation/ties. P3c adds no SIMD or FMA dispatch; rotated approximate
scoring is a separate reference. P3b can batch bounded f16 decode into
existing `Scorer`; a later native-f16 kernel requires its own P3d acceptance.

P5 `0x0400` summaries retain separate source binding and ordinal mapping;
map their source-row eligibility into ANN node ordinals through checked
IDs/source rows. Ineligible/tombstoned nodes can bridge; never prune the
traversal frontier just because a predicate fails. Eligibility controls
result heaps, not graph connectivity. Keep exact bitmap/scan fallback
exhaustive, bounded approximate widened ef/work and explicit
shortfall/budget-exhausted status; no total-work claim from ef alone.

Hybrid vector/text legs and their final recheck share captured T/clock.
Named-vector ANN remains volatile but consumes bounded named-vector views,
with current primary-record visibility; no P3c primary descriptor is
misused for a model version. Graph retrieval obtains pivots from the same
hybrid API and maps public IDs to attachments; lazy evidence alone may
materialize returned records. Gap 1 recall/consolidation uses these
existing retrieval boundaries and never treats ANN as authoritative memory
or deletion state. Gap 4 valid-time/as-of selectors and graph TTL remain
independent of vector proximity; historical graph selection does not turn
current vector ANN into a historical-vector index. No new wire/schema or
temporal-vector semantics are implied.

## H. Test plan (future implementation, not run here)

Propose `tests/compact_ann_oracle.py` and
`tests/fixtures/compact/ann-v1/` with independently assembled hex frames,
binding transcripts, source bits and malformed-case manifests. Python must
implement envelope/sections/SHA streams/SplitMix rejection/permutations/
rotation/signs/scalar codec itself, not call Rust to generate expectations.
Use explicit binary64 rounding after each operation and a rational/Decimal
cross-check for boundary signs, rather than a BLAS dot/rotation. Include
known RNG streams, rejected RNG draws and all odd/small dimensions. Golden
byte fixtures are committed/reviewed before codec code. Oracle self-test
must fail on deliberately changed expected bit, ordinal and source digest;
round-tripping Rust output alone is insufficient.

| Named future test(s) | Required evidence |
|---|---|
| `compact_ann_v1_independent_golden`, `compact_ann_oracle_detects_mutation` | Independent source/descriptor/topology/scalar/rotated bytes, both precision codes, known random/sign/term bits, 768 sign bytes exactly 96, no hidden 1,024-bit padding. |
| `compact_ann_rejects_malformed_before_allocation` | Every count/length/offset overflow, cap, duplicate/missing run, out-of-range/wrong-level/self/duplicate neighbor, invalid entry, padding/trailing bytes, truncated image/end, NaN and wrong norm/correction fail at the correct understood boundary. |
| `compact_ann_reopen_matches_immediate`, `compact_ann_rebuild_bytes_deterministic` | Same compact IDs/distance bits at same clock before/after persistence, thread counts 1/many, plain/misaligned/file-buffer backings; canonical payload bytes even when encrypted ciphertext differs. |
| `compact_ann_full_binding_not_prefix`, `compact_ann_stale_not_corrupt`, `compact_ann_unsupported_set_is_atomic` | Same shape/eight-byte collision simulation does not bypass full B/G/C/config/seed; foreign valid set is stale, source-matching bad code is corrupt; unknown/unsupported version never partly installs. |
| `compact_ann_overlay_shadows_all_changes`, `compact_ann_expired_put_never_reveals_base`, `compact_ann_overflow_falls_back` | Insert/update/delete/metadata-only/TTL-only, duplicate-ID batches, conditional writes, expiry==clock, cosine-zero replacement, delete/reinsert; exact overlay merge/dedup/current rerank, no durable-write failure or truncated buffer at bound+1. |
| `compact_ann_filtered_bridges_survive`, `compact_ann_hybrid_graph_same_view` | P5 eligible bitmap cannot remove bridges; counters/shortfall and strict fallback remain; hybrid and graph evidence share visibility/clock, temporal selector independent. |
| `compact_ann_checkpoint_never_waits`, `compact_ann_snapshot_retirement`, `compact_ann_cancel_and_publish_race` | Checkpoint omission/state-0 path, mid-build generation change, catch-up hole, cancelled/panicked worker, old snapshot after DB drop, Windows last-pin unmap/unlink, no stale publication. |
| `compact_ann_cache_bound_with_pins`, `compact_ann_encrypted_never_spills` | Reserve-before-decrypt, cache/transient/concurrent accounting, deterministic eviction, cross-generation same-shape isolation, tamper/auth failure, wrong/missing keys, no plaintext sidecar/temp file. |
| `compact_ann_lifecycle_matrix`, `compact_ann_report_once` | Copy versus rewritten backup/restore/convert/re-encrypt/replica/feed/export, refusal parity, rebinding or omission, corruption not repair; all readers validate, reports bounded/once, no worker in inspection/feed/export. |
| `compact_ann_reference_numeric_edges`, `compact_ann_exact_rerank_oracle` | Signed zeros, subnormals, extreme finite stored values, empty/zero cosine, odd dimensions, scalar ties/constants/clamps; exact returned distances/tie order match stored-value oracle on every supported P3d kernel. |
| `compact_ann_private_gate_stays_closed`, `compact_ann_old_build_refusal` | No default/public activation or wire/API diff; old pre-compact reader refuses flag before replay/repair, current release reader still refuses unqualified source capability, compact-aware unknown-derived reader skips only authentic optional data. |
| `compact_hnsw_rerank_uses_pinned_generation`, `compact_rebuild_snapshot_survives_drop_and_refused_checkpoint`, `compact_ann_snapshot_rejects_other_generation` | Moved here from the P3 slice 4 row (OPEN-71-04, 2026-11-05) and required before any compact ANN activation: a rebuild or HNSW rerank reads the one pinned generation across a Database drop and a refused checkpoint attempt, and an ANN snapshot built for one generation is refused by another. P3b slice 4 keeps the generic pin and the refusal; no gate disappears with the move. |

Future build coverage: default and `--no-default-features`, WASM owned-buffer
path with no worker thread, native supported architectures; unavailable
threads/CPU instructions never cause implicit activation or plaintext spill.
These are plans, not commands run for this docs-only slice. Approximate
recall is evaluated only in F.2, not asserted by deterministic codec tests.

## I. Ordered implementation sub-slices and conflict domains

P3c **implementation starts only after `p3b-engine-integration` is
independently accepted**, except for the isolated prototype work explicitly
permitted below against accepted P3b slice 0. Owner design approval does not
waive any code-slice acceptance or integration prerequisite. The
P3b design's five numbered slices are the dependency vocabulary here.
P3b's accepted slice-0 source generation may refine that order;
do not equate it with the already accepted storage-contract slice 0.
LOC ranges include implementation plus focused tests, not performance or
on-disk size estimates.

| Slice | Exact proposed touch points and acceptance | Size / dependencies / regression mutation |
|---|---|---|
| C1. Private codec and independent oracle | New `src/compact/ann.rs::{check,encode,bind}` and `tests/compact_ann_oracle.py`/fixtures; reuse `quantize::ScalarU8`; add flat checked topology access. No shared dispatch until storage owner integrates. Golden/malformed/full binding tests in H pass without Record materialization. | 700-1,100 LOC; integration after engine-integration acceptance and P3b slice 1 generation seam freeze; bounded pure-codec work may run earlier **against the accepted P3b slice-0 source generation**, with no integration or activation. Mutation: accept neighbor ordinal N; malformed test must fail. |
| C2. Flat build/search and overlay | `src/hnsw.rs::Index::{construct,plan,link,prune,node_distance,filtered_layer,finish_graph_hits,merge_buffered,ready_for,memory}`, `QuantizedSnapshot::encode`; `src/quantize.rs::ScalarU8::{train,encode_into,norm,distance}` adapters; `src/records.rs` read seam, `src/collection.rs::{apply_committed,apply_metadata_committed}`. New private flat arena/handle, preserve resident path. Immediate/reopen-independent search and overlay/expiry/filtered bridge tests pass. | 650-1,100 LOC; P3b slices 1-3 and slice 4 ANN adapters. Engine-domain edits serialize with P5/HNSW and P3b consumer work. Mutation: omit metadata-only affected ID; overlay test fails. |
| C3. Storage read/write and reporting | Storage owner only: `src/compact/mod.rs::ImageCheck::{prefix,block,end}`, `skip_report`, table admission; `src/format.rs::{load_inner,apply_supplement}`; `src/maintenance.rs::write_v5_image`; `src/directory.rs::Chain::install`; `src/changes.rs::scan`, `src/logops.rs` image/export validation. P3 owner supplies C1 functions, no direct shared-owner edits. Eager validation, complete set installation, state-0/omit checkpoint, bounded once-only outcomes, all-reader parity. | 450-800 LOC; C1-C2 and P3b slice 5 pin/publication/maintenance. No early implementation against slice 0 alone. Mutation: ignore authentication error on derived block; encryption/lifecycle test fails. |
| C4. Worker and lifecycle | `src/rebuild.rs::Job::{start_inner,track,finish,stop}`, limits; `src/lib.rs::{start_rebuild_with_options_in,poll_rebuild_in,maintain_background_hnsw_in,start_checkpoint,finish_checkpoint,search_ready_with_options_in}`; `src/quality.rs::{ready_index,ann_memory,ann_quality}` and private cache/accounting in `src/compact/ann.rs`. Storage owner handles lifecycle hooks in `maintenance`, `directory`, `logops` and replica replacement. Ready/pending/retirement/budget/failure matrices pass with no synchronous compact build. | 500-900 LOC; C2-C3, all P3b lifecycle slices. Could prototype worker pin/cancel tests earlier against **accepted** slice 0, not integrate/search before dependencies. Mutation: publish after source token change; race test fails. |
| C5. Rotated private prototype | `src/quantize.rs` new `RotatedOneBitV1::{encode,score}` and settings; `src/compact/ann.rs` parameters/terms checker; `src/hnsw.rs` private profile dispatch; bounded source batches through existing `exact_kernels::Scorer`, no kernel edits. Numeric/golden/no-padding/gate and exact-rerank tests pass, independent review before integration; no recall/default claim. | 400-700 LOC; C1-C4 for integration. Scalar transform/oracle definition may start after design acceptance and an accepted P3b slice-0 shape; shared engine integration still waits for its dependencies. Mutation: round code stride up to next power of two; 768 golden test fails. |

Every sub-slice first demonstrates its named failing mutation, restores it
and records the green regression; each returns a bounded candidate for
independent acceptance. Later P3d owns full qualification/lifecycle acceptance
and the unchanged major-release measurement campaign.

Conflict domains are explicit: storage owner exclusively edits
`src/compact/**` shared registration, `src/format.rs`, `src/maintenance.rs`,
`src/directory.rs`, `src/changes.rs`, `src/logops.rs` and `src/lib.rs`
storage hooks. P3 codec author may own `src/compact/ann.rs` only by reservation
with that owner; no concurrent shared match/table edits. Engine files are
`hnsw.rs`, `quantize.rs`, `ann_snapshot.rs`, `disk_ann.rs`, `rebuild.rs`,
`records.rs`, `collection.rs`, `quality.rs`, `planner.rs`, `metadata_index.rs`,
`text_index.rs`, `vectors.rs`, `hybrid.rs` and graph retrieval adapters.
Reserve function ownership with P3b/P5 before editing; P4 and Gap 4 share
checkpoint/feed branches. Source access lifetime first, codec next,
serialized dispatch/publication last; no merge-by-overlapping-patches plan.

## J. Owner decisions and conflict register

The owner explicitly approved each choice separately on **2026-11-02**.
Implementation still requires independent code-slice acceptance and the
stated dependencies. These decisions do not activate any feature or
authorize a release, benchmark campaign or performance claim.

1. **Kinds/bounds:** approved `0x0200-0x0203`, v1, optional; primary only,
   4 MiB hard body limit, canonical complete-row partitions and <=1,000,000
   nodes. Oversize accelerator refuses explicitly and retains source;
   no source limit/default change.
2. **Private selection/persistence:** approved advisory settings in
   `0x0200`, state-0 descriptor across compact checkpoints, private one-bit
   constructor/search only. Public `AnnOptions`/`Quantization` unchanged.
   The derived preference may be dropped by readers/rewrites that skip
   optional data. **If persistent user-facing one-bit policy is required,
   this is blocker B1:** authoritative settings in a derived optional kind
   conflict with contract sections 3/6 and current settings-v1 decoding.
   Recommended resolution is a separate storage/API amendment before such
   a promise, not an op-7 extension or public enum addition here.
3. **Binding:** approved full B/G/C and config/precision/versions/seeds,
   eight-byte B prefix only as guard; no whole-image digest cycle.
   Source-only cursor and source physical revisions/expiry must be frozen
   with P3b before code.
4. **Overlay pressure:** approved 256 IDs/8 MiB compact readiness bound,
   all mutations shadow base, exact fallback and background/checkpoint
   scheduling. No durable write rejection and no restriction of resident
   background behavior; storage overlay bounds remain separate.
5. **Build/checkpoint:** approved build immutable base S, delta to T,
   no query construction/wait; checkpoint carries revalidated compatible
   complete set or pending descriptor. Rebinding only after full checks,
   not a digest patch. One worker and existing failure/status taxonomy.
6. **Rotation:** approved seeded eight-round signed rational pair rotations,
   no dimension expansion, fixed binary64 scalar reference and all four
   row terms; no RaBitQ/unbiasedness label. Rerank default four with current
   bounded ef; exact stored-value scores only.
7. **Memory/encryption:** approved shared 16 MiB decrypted ANN cache,
   <=4 MiB bodies, <=two transient buffers/operation, explicit reservations
   and 256 MiB worker arena ceiling subject to existing stricter limits.
   No plaintext sidecar/spill; no GPU or P3d dispatch changes.
8. **Diagnostics/lifecycle:** approved bounded mixed per-group skip
   outcomes and actual queued/deferred/none rebuild status, same log target/
   server path, eager checks on all readers. Feed/export never start jobs.
   Storage owner implements this prescribed codec-era reporting transition.
9. **Dependencies/public surface:** approved the proposed private read
   seam gaps and source-only identity cursor review with P3b, then C1-C5
   ordering. No new public items or `Error` variants in this candidate.
   Existing precision API approval is not new P3c API approval.

**Current conflict register:** no unconditional conflict with P3 or the
storage contract is asserted for the bounded private candidate above.
B1 remains a conditional blocker for any future durable user-facing policy:
the owner accepted optional preference loss for this private candidate, so
no such amendment is approved or required by that choice. It must not be
silently resolved by calling the setting source policy.
Eager validation deliberately accepts open-time work rather than
relaxing contract decision 4. In-image persistence/no encrypted sidecars
implements decision 6; private-first selection implements decision 7.
P3b acceptance and source seam gaps are prerequisite blockers, not proof of
an accepted source-generation implementation.

**Proposed surface:** all new items are crate-private: `CompactAnnSettingsV1`,
`CompactAnnHandle`, checked flat topology/partition views, rotation reference,
codec `check/encode/bind`, bounded cache and worker accounting, plus G's
`ReadAccess` additions. No public additions are necessary for this private
prototype. If B1 later requires a public opt-in, list exact signatures and
exhaustive enum compatibility in a separate reviewed proposal, including
REG-001 `tests/fixtures/api_surface/vlite-default.txt` and matching
`site/api.html` `data-api` obligations. Do not run
`scripts/api_surface.py baseline` to conceal changes. OpenAPI, proto and all
wire/binding fields remain frozen for this slice.

## K. Risks and unverified items

- **Reconciled at v0.7.0.** P3b slice 0 is accepted and integrated, with
  slices 1a, 1b, 2 and 3 and lane B's seam. A test-gated generation pin
  (`CompactGeneration`, `CompactView`), a base-plus-overlay `CollectionRecords`,
  an ordered base-plus-overlay cursor (`compact_iter`) and a bounded read
  workspace (`ReadWorkspace`) exist; there is no `ReadAccess` trait, and the
  seam compiles only under `cfg(test)`. Added by lane R0's slice 3 (written during 0.7.1, carried to 0.7.2): the
  physical-base cursor `CompactView::base_rows`, the affected-ID cursor
  `CollectionRecords::affected_ids` (the whole overlay, without an `S`: the floor
  and lineage check waits for the binding) and the non-materializing
  `CollectionRecords::ordered_ids`, which E4's compact exact scan walks. Still absent: a portable binding and a
  fallible bounded batch scorer (base-only vectors exist as
  `CompactView::read_vector`). G's assumptions are restated with concrete shapes
  in the [seam freeze](p3b-seam-freeze.md#5-additions-for-p3c), which the owner
  approved as additive shapes on 2026-11-05. The binding and the batch scorer
  stay with the ANN lane.
- Current `ImageCheck` retains some validation state and current loader
  materializes sources. Eager full ANN source/code checks must not force a
  resident record map. Peak validation, rebuild arena and aggregate concurrent
  cache/scratch costs are unmeasured. Format maxima do not guarantee
  admission or S2 qualification.
- The rotation is fully specified but its recall, numerical cross-platform
  reproducibility, build cost and cache behavior are unverified. Correctly
  rounded sqrt/separate operations need independent native/WASM fixtures;
  approximate scoring never inherits P3d exactness by analogy.
- Sorted topology lists define compact ordering, not proven identity with
  resident HNSW recall under ties. Stable immediate/reopen behavior and
  exact final score bits are required separately from recall measurements.
- Descriptor rebinding without changing topology is allowed only for an
  identical eligible row set/code source. Whether the checkpoint pipeline
  can cheaply establish that is unverified; pending-descriptor omission is
  the safe planned path, not a timing claim.
- Existing public memory reports assume resident f32 and have no separate
  mapped/cache fields. Private accounting/logs must not mislabel source
  bytes as resident or add a public field silently. One-bit public
  observability/selection remains intentionally unavailable.
- Backup/restore/key-rotation/replica parity over the proposed codec and
  deferred Windows retirement do not exist yet. Current source-side gate
  refusal must remain until those independently owned paths qualify.
  No new production activation, release waiver or measurement is implied.
