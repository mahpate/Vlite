# Performance improvement proposal

| Field | Value |
|---|---|
| Status | Proposal. Not approved or scheduled; nothing here is implemented |
| Created | 2026-10-27 |
| Scope | Vector and property-graph performance |
| Baseline | DO-005, release 0.6.0 on Azure Standard_D16s_v5 ([`baseline.md`](../feature-agent-graph/baseline.md)) |
| Benchmark cadence | Benchmark campaigns run for major releases only ([`decisions.md`](../feature-agent-graph/decisions.md)) |

This document collects options for closing the measured performance gaps. It
offers three levels of change, from least to most disruptive:

1. [Compatible improvements](#option-a-compatible-improvements): no file-format
   or API break.
2. [File format v5](#option-b-file-format-v5): a new opt-in format; old files
   still open.
3. [Clean break](#option-c-clean-break-vlite-10): no backward compatibility.

Gains quoted below are design targets, not measurements. Every change must be
measured under the existing benchmark rules before it is claimed.

## Where VLite stands

The competitor numbers come from the archived multi-engine runs on
[`compare.html`](../../../site/compare.html) and
[`RESULTS.md`](../../../benchmarks/RESULTS.md). Those runs used older VLite
builds on WSL2, a 16-vCPU Xeon 8370C. Release 0.6.0 has only been measured
against its own previous baseline, so the gaps below are estimates.

### Vector search

| Area | Evidence | Gap |
|---|---|---|
| HNSW query latency | 100k × 768: old VLite 3.5 ms p50 vs zvec 0.9 ms, FAISS 0.5 ms. 0.6.0: 2.14 ms at 384 dimensions | Probably still 2–4× behind |
| HNSW build | Old VLite 525 s vs zvec 7.75 s at 100k × 768. 0.6.0: 18 s at 384 dimensions | Much better, likely still behind |
| Exact search | 0.6.0 is 4–6% slower than `81ff901`. Old VLite 32 ms vs FAISS 21.6 ms at 768 dimensions | Regression plus a gap |
| Recall | 0.26 at search ef=64 on uniform random data (a stress setting); 0.995 on real GloVe embeddings in release 0.2.0 | No matched-recall comparison with other engines |
| Server latency | REST about 1.5× slower than Qdrant on unfiltered queries | Serialization and HTTP overhead |
| Writes under load | Lazy rebuilds stall queries for about 3.7 s | Tail latency |
| Memory | HNSW peak working set +12% in 0.6.0 | Regression |

Strengths to keep: far smaller files (10 MiB vs Qdrant 196 MiB at 20k × 128),
faster filtered exact search than Qdrant, a single-file store, and ingest 5–6×
faster than `81ff901`.

### Property graph

No competing graph engine has been measured. Figures are from
[`graph-measurements.md`](../feature-agent-graph/graph-measurements.md), at
100,000 nodes and 1,000,000 edges.

| Area | Evidence | Gap |
|---|---|---|
| Memory per edge | About 1,016 B per edge, 15.5× the size of the log | Largest gap |
| Build and load | Build 18.1 s (about 55k edges/s); replay 3.5 s | Slow bulk load and reopen |
| Reads | `neighbors` p99 70 µs; two-hop `traverse` p99 0.68 ms | Already good |
| Not measured | Property-index memory, combined vector+graph retrieval, Kùzu/Neo4j comparison | Candidates for the next major-release benchmark |

## Option A: compatible improvements

### Rules

- Keep the on-disk record format and the existing operation codes unchanged.
  Every existing `.vlite` file must open, and files must stay readable by
  0.6.0.
- Put speed-ups only in data that can be rebuilt: indexes, caches and
  snapshots. Version them separately (for example `ann_snapshot` v1 → v2). If a
  snapshot is missing or newer than the reader understands, rebuild it instead
  of failing.
- Make new behaviour opt-in through a config option, a new parameter or a Cargo
  feature. Keep current defaults, REST/OpenAPI fields and SDK signatures. Add
  optional fields only; the OpenAPI golden file needs approval.
- Keep exact search results identical. `src/lib.rs` already guarantees
  identical sums with or without SIMD.

### Vector search

1. **Exact search.** Profile before changing anything. Likely sources are the
   deterministic summation without FMA and per-call overhead. Options:
   AVX-512, a separate faster opt-in mode, and pre-normalised vectors for
   cosine. Exact top-k IDs must not change.
2. **HNSW query latency.** Use quantization (SQ8/PQ) to pick candidates, then
   re-rank with full vectors. Store a flat neighbour layout in an
   `ann_snapshot` v2 to cut memory jumps, and prefetch neighbour vectors.
   Snapshot v1 stays readable.
3. **Recall.** Keep the default `ef`. Add an opt-in target-recall or auto-`ef`
   setting, and publish an `ef` sweep on real embeddings.
4. **HNSW build.** Tune the existing parallel build (batched inserts, fewer
   locks). Save the built graph so a reopen loads it instead of rebuilding.
5. **Writes under load.** Make "use the ready index, rebuild in the
   background" the opt-in default for new collections. Existing collections
   keep their current behaviour.
6. **Memory.** Store the graph in compact u32 arrays, optionally keep
   quantized vectors in memory, and memory-map snapshots. All three are derived
   data, so the file format is untouched.
7. **Server.** Add an optional binary or columnar response format, keep
   connections alive, and serialize JSON with fewer copies. JSON stays the
   default.

### Property graph

1. **Memory per edge.** Change only the in-memory layout: CSR-style adjacency,
   interned labels and relationship types, small integer IDs and packed
   properties. The write log stays the same, so old files load into the new
   layout.
2. **Build and load.** Replay changes in parallel per graph, pre-size tables,
   and add an optional checkpoint snapshot of graph state that is checked
   against the log and rebuilt if it doesn't match.
3. **Reads.** Keep the current limits and result ordering. Add optional
   caching only for repeated traversals.

## Option B: file format v5

The current default format is v3 (`CURRENT_VERSION` in `src/format.rs`). A v4
format already exists: `header_v4` adds a lineage ID and a collection-ID floor,
and `batch_frame_v4` adds the resumable import checkpoints that `vlite-import`
requires. The new format is therefore v5.

### Design

v3 and v4 are a single log of frames, and all state is rebuilt by replaying it.
v5 keeps a short write-ahead log and adds immutable, checksummed segments.
Each segment is page-aligned and memory-mappable. A small manifest lists the
live segments and is switched atomically. It is still one file, and still
crash-safe.

### Vector search

1. **Columnar, aligned vector blocks.** Store vectors in 64-byte-aligned blocks
   per collection, separate from metadata. Exact scans read them straight from
   the mapped file without decoding or copying. Exact search should get
   faster, reopen should become near-instant because nothing is replayed, and
   memory use drops because the OS page cache is shared.
2. **Saved ANN graph.** Store the HNSW (or a disk-friendly Vamana-style graph)
   as a flat adjacency array in a segment. The 18 s build happens once, at
   segment flush, and a reopen runs no build.
3. **Built-in quantization.** Store SQ8, PQ or binary codes beside the
   full-precision vectors. Search on the codes, then re-rank with full
   vectors. Target: 2–4× faster HNSW queries and 4–32× less memory for the
   in-memory part. Full precision is kept, so exact results don't change.
4. **Per-segment filter indexes.** Bitmaps and min/max statistics let filtered
   search skip whole segments.
5. **Compaction instead of global rebuilds.** Writes go to a small mutable
   segment searched by brute force or a small graph. Background merges build
   the large index. This removes the stalls under writes.
6. **Optional compression** (zstd or bit-packing) for metadata and attachments.

### Property graph

1. **CSR adjacency segments.** Sorted node and edge ID arrays with offsets, in
   both directions, with relationship-type columns. Target: under 100 B per
   edge, memory-mapped, so retained memory is close to zero.
2. **Dictionary-encoded** labels, types and keys, and columnar properties with
   null bitmaps.
3. **Graph checkpoints.** Opening a file loads the CSR segment and replays only
   the log written after it, instead of all edges.
4. **Saved property and edge indexes** instead of rebuilding them in memory.
5. **One internal ID space** for records and nodes, so combined vector+graph
   retrieval (EPIC-010) is a join on array positions.

### Format rules

- Every segment type is versioned and carries a "feature flags required"
  bitmap, so readers safely refuse what they don't support. Later versions
  become additive.
- Checksum each page; keep a single atomic manifest switch.
- Use the same layout on all platforms (little-endian, fixed alignment), so
  files stay portable, including to WASM.

### Compatibility and migration

- v5 binaries keep reading v1–v4 files through the existing frozen code paths.
- `vlite-maintain upgrade --to v5` writes a new file and keeps the original.
  It verifies record counts and checksums and compares exact-search results
  before swapping. The upgrade must carry over the v4 lineage ID and
  collection-ID floor, so resumed imports and file identity still work.
- Opening an older file never upgrades it silently.
- An older binary given a v5 file returns a clear "unsupported format version"
  error and never modifies it.
- REST, gRPC, SDK and OpenAPI stay unchanged. New settings (quantization,
  segment size) are optional fields.
- Rollout: about two releases with v5 available but off by default, then on by
  default for new files at a major release, then deprecate writing older
  formats. The readers stay.

### Trade-offs

- Months of work, mainly compaction and crash consistency.
- Slightly more write amplification.
- Torture and power-loss tests must be extended to segments and the manifest.

**Suggested order:** graph CSR checkpoints (largest gap), then a saved ANN
graph with quantization, then columnar vectors, then compaction.

## Option C: clean break (VLite 1.0)

This option drops backward compatibility entirely.

### Storage engine

- **No log replay on open.** Immutable, memory-mapped, page-aligned segments
  plus a small write-ahead log, switched by a manifest. Opening a file maps it
  instead of rebuilding state, so reopen drops from 240 ms toward a few
  milliseconds.
- **Columns per collection:** vectors, IDs, metadata, filter bitmaps and
  statistics stored separately.
- **Lower-precision vectors by default:** float16 or bfloat16 for re-ranking,
  plus binary or 4-bit codes for candidate search. Full float32 becomes
  optional. Memory and disk shrink about 2–8×.
- **One ID space** for records, nodes and edges (dense u32), so vector–graph
  joins are lookups by position.
- **Remove old cost centres:** v1–v4 frame types, synchronous lazy rebuilds and
  per-record attachments stored inline.

### Vector search

- A disk-friendly ANN index (Vamana/DiskANN, or IVF with residual PQ) replaces
  HNSW. It is built per segment, merged by compaction and saved to disk, so no
  index is rebuilt at query time.
- Quantized search with re-ranking as the only ANN path, with automatic `ef`
  or probe tuning towards a recall target.
- SIMD kernels with FMA (AVX-512, NEON, WASM SIMD) on aligned blocks. Exact
  search drops bit-for-bit identical results across CPUs and guarantees only
  identical top-k ranking within a small tolerance.
- Filters inside the index: segment bitmaps and filter-aware traversal replace
  post-filtering.
- Writes never block queries: a small mutable buffer is searched by brute
  force and compaction runs in the background.

### Property graph

- CSR adjacency as the main storage, not a derived index. Edges sorted per node
  and type, in both directions, with delta and varint compression. Target:
  about 8–24 B per edge.
- Columnar properties, dictionary-encoded labels and types, saved property
  indexes.
- Parallel bulk loading (sort, then build the CSR) instead of inserting one
  edge at a time. Target: millions of edges per second.
- Vectorised traversal: expand a whole hop at once using frontier bitmaps.

### API and server

- A new `/v2` API with fewer copies (Arrow IPC or protobuf), batch-first
  endpoints and streaming results.
- Remove legacy request shapes and old SDK signatures.
- Each reader pins a consistent snapshot (MVCC), so no locks are needed.

### Costs and risks

- Every existing file needs a one-time export and import; every SDK and
  integration needs updating.
- About 6–12 months of engine work.
- Crash-safety and power-loss qualification must be redone; the Azure Files
  caveat returns.
- The benchmark baseline restarts and earlier results stop being comparable.

## Recommendation

Start with Option A, because it is low risk and fixes the regressions. Plan
Option B for the next major release; it reaches most of the gains while
existing files keep working. Reserve Option C for VLite 1.0, keeping the
v1–v4 reader only for importing old files. The gains that only a clean break
delivers are lower-precision storage by default and the redesigned API.

## Guardrails for every change

- Read old-format fixture files on every test run.
- Read old files with the new build, and new files with the old build where
  the format allows it.
- Check the OpenAPI golden file and SDK contracts.
- Compare recall and latency at equal settings, never latency alone.
- Measure gains at the next major-release benchmark under the unchanged quiet
  gate (background CPU mean ≤10%, no half-second sample above 50%).
