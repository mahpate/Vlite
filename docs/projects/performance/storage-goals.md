# Storage and speed goals

| Field | Value |
|---|---|
| Status | Goals approved 2026-10-30. Work order and progress in [`next-major.md`](../roadmap/next-major.md); no goal counts as met until the major-release DO-005 run measures it |
| Created | 2026-10-29 |
| Replaces | Options B and C of [`performance-proposal.md`](performance-proposal.md). Option A there still applies to today's format |
| Key decision | VLite no longer has to keep a database in one file |
| Baseline | DO-005, release 0.6.0 ([`baseline.md`](../feature-agent-graph/baseline.md)); competitor runs in [`RESULTS.md`](../../../benchmarks/RESULTS.md) and [`compare.html`](../../../site/compare.html) |
| Related | Feature gaps beyond speed and size: [`product-gaps.md`](../product-gaps/product-gaps.md) |

## Why revisit

The earlier proposal assumed one `.vlite` file. Dropping that requirement
lets VLite use the same layout the fastest stores use: a directory with a
write-ahead log, immutable segment files and a manifest. This document sets
measurable goals first, then lists what to borrow from other stores to reach
them.

## Goals

Every goal is measured at equal recall, on the DO-005 host, under the
unchanged quiet gate (background CPU mean ≤10%, no half-second sample above
50%). Competitor figures are the archived runs; they are re-measured with the
same build before any claim is made.

### Smaller

| ID | Goal | Today | Target |
|---|---|---|---|
| S1 | Disk per 768-d vector, default settings | 3,072 B of float32 plus record overhead | ≤1,700 B: float16 stored vectors and a 96 B quantized code |
| S2 | Resident memory per vector for ANN search | Full float32 vectors and the HNSW graph | ≤25% of today: codes and graph in memory, full vectors read from mapped files only for re-ranking |
| S3 | Graph disk per edge | Log records, about 65 B per edge | ≤24 B per edge |
| S4 | Graph memory per edge | About 1,016 B | ≤100 B |
| S5 | Metadata and attachments | Stored raw in each record | ≥3× smaller on typical JSON |
| S6 | Space after deletes and TTL expiry | Reclaimed only by offline compaction | Reclaimed automatically by background merges, within one merge cycle |

Float32 stays available as an opt-in for users who need bit-exact stored
values. Smaller stored vectors change stored values, so S1's default is a
user decision (see [Decisions needed](#decisions-needed)).

### Faster

| ID | Goal | Today | Target |
|---|---|---|---|
| F1 | ANN query p50, 100k × 768, recall@10 ≥0.95 | Old VLite 3.5 ms; zvec 0.9 ms; FAISS 0.5 ms | ≤1.0 ms (zvec parity) |
| F2 | Exact search p50, 100k × 768 | Old VLite 32 ms; FAISS 21.6 ms; 0.6.0 is 4–6% slower than `81ff901` | ≤22 ms (FAISS parity), regression removed |
| F3 | ANN index build, 100k × 768 | 0.6.0 18 s at 384-d; zvec 7.75 s at 768-d | ≤2× zvec at 768-d |
| F4 | Reopen time | Replays the whole log; grows with data (240 ms on the baseline) | ≤50 ms, independent of data size |
| F5 | Query stalls during writes | About 3.7 s during lazy rebuilds | No query blocked >10 ms by indexing |
| F6 | Graph bulk load | About 55k edges/s; replay 3.5 s for 1M edges | ≥1M edges/s; reopen ≤300 ms for 1M edges |
| F7 | REST query latency | About 1.5× slower than Qdrant, unfiltered | At or below Qdrant on the same harness |
| F8 | Filtered search | Faster than Qdrant on exact | Keep the lead, and add filter-aware ANN |

### Must not regress

- Crash safety: every acknowledged write survives a process crash; durability
  modes keep their documented contracts.
- Existing features keep working: TTL, change feed, read replicas,
  point-in-time restore, encryption at rest and key rotation, graph, WASM and
  GPU builds.
- Exact search stays exact. Any new fast path that changes results is a
  separate, opt-in mode.
- Old `.vlite` files keep opening; conversion is explicit and keeps the
  original.

## What to borrow

| Technique | Borrowed from | VLite goals | What VLite does with it |
|---|---|---|---|
| Directory store: WAL + immutable segments + manifest | Qdrant segments, Milvus growing/sealed segments, LanceDB, RocksDB | F4, F5, S6 | Writes go to the WAL and a small mutable segment. Full segments become immutable files. A manifest names the live segments and is replaced atomically. |
| Versioned manifests | LanceDB, Delta Lake | Must-not-regress | Each commit writes a new manifest version. Point-in-time restore and read snapshots become "open manifest N" instead of a full copy. |
| Memory-mapped, aligned columns | zvec (Arrow + mmap), LanceDB, DuckDB | F2, F4, S2 | Vectors, IDs and metadata are separate 64-byte-aligned columns. Search reads them straight from mapped files. |
| Quantized search with re-ranking | Qdrant (scalar, binary), Weaviate and LanceDB (RaBitQ), Elastic BBQ, FAISS (PQ) | F1, S1, S2 | Keep 1-bit or 8-bit codes in memory, search them, then re-rank the top candidates with full-precision vectors. |
| float16/bfloat16 storage | pgvector `halfvec`, Milvus, Qdrant | S1 | Default stored precision for new collections, if approved. |
| Disk-friendly ANN graph | DiskANN/Vamana (Milvus, SQL Server), zvec/Proxima HNSW | F1, F3, S2 | Build the graph once per sealed segment, store it as a flat array, and load it by mapping the file. |
| IVF partitions for large segments | FAISS, LanceDB IVF-PQ, zvec | F3 | Optional for large, rarely-updated collections; much faster to build than HNSW. |
| Background merges | Qdrant optimizers, Milvus compaction, LSM trees | F5, S6 | Merge small segments, drop deleted and expired records, and build the large index off the query path. |
| Roaring bitmaps | zvec, Milvus, Lucene | F8, S6 | Deletes, TTL expiry and filter results as compressed bitmaps per segment. |
| Per-segment filter indexes and statistics | Qdrant payload index, Lance, Parquet | F8 | Min/max statistics and bitmaps skip whole segments; filter-aware graph traversal replaces post-filtering. |
| Lightweight column compression | DuckDB (bit-packing, frame-of-reference, FSST), Parquet + zstd | S5 | Compress metadata and attachments per column. |
| CSR adjacency with compressed IDs | Kùzu, DuckDB graph extensions | S3, S4, F6 | Edges sorted per node and type in both directions, delta- and varint-encoded, memory-mapped. |
| Sort-then-build bulk load | Kùzu `COPY`, Neo4j admin import | F6 | Build CSR segments from sorted batches in parallel, instead of inserting one edge at a time. |
| SIMD distance kernels with FMA | FAISS, zvec, SimSIMD | F1, F2 | Runtime-selected AVX-512, AVX2, NEON and WASM SIMD on aligned blocks. Bit-exact summation stays the default for exact search. |
| Binary response format | Qdrant gRPC, LanceDB Arrow IPC | F7 | Optional Arrow IPC or protobuf responses; JSON stays the default. |

## Directory layout (sketch)

```text
mydb.vlite/
  MANIFEST-000042        live segments, schema and lineage; replaced atomically
  CURRENT                points to the newest manifest
  wal/000123.log         recent writes, replayed on open
  segments/<id>/         one immutable segment
    vectors.f16          aligned vector column
    codes.bq             quantized codes
    ids, meta.zst        IDs and compressed metadata
    deletes.roaring      deleted and expired records
    ann.graph            saved ANN graph
  graph/<id>.csr         graph adjacency segments
```

Encryption is applied per segment and per WAL file, with the key ID recorded
in the manifest. A single-file copy remains available through an explicit
pack/unpack command for backup and transfer.

## Phases

Each phase ships on its own and has measurable exit criteria.

1. **Quick wins on today's format** (Option A of the proposal). Remove the
   exact-search regression (F2), background index rebuilds (F5), compact
   in-memory graph (S4). No format change.
2. **Directory store foundation.** WAL, immutable segments, atomic manifest,
   background merges, crash and power-loss tests. Exit: F4, S6 and the
   must-not-regress list pass; explicit converter from `.vlite` files.
3. **Compact vectors.** float16 storage, quantized codes, saved ANN graph per
   segment, SIMD kernels. Exit: F1, F3, S1, S2.
4. **Compact graph.** CSR segments, dictionary-encoded labels and properties,
   parallel bulk load. Exit: S3, S4, F6.
5. **Filters and server.** Segment statistics and bitmaps, filter-aware ANN,
   optional binary responses. Exit: F7, F8, S5.

Benchmarks are run at major releases only; phases 2–5 together are expected
to form the next major release.

## Costs and risks

- More files to manage; hosted deployments must mount a directory instead of
  one file. Azure Files stays experimental until power-loss qualified.
- Crash-safety and power-loss qualification must be redone for the WAL,
  segments and manifest.
- Replicas, change feed and restore move from file copies to segment and
  manifest shipping.
- Quantized and float16 defaults trade bit-exact stored values for size.

## Decisions

Approved 2026-10-30: all goals (S1–S6, F1–F8, must-not-regress) and all
phases.

1. **Approved:** the directory store is the format for new databases. Old
   `.vlite` files stay readable and are converted only on request.
2. **Open:** default stored vector precision for new collections: float16
   (smaller, slightly changed stored values) or float32 (unchanged, larger).
   Needed before phase 3.
3. **Open:** whether parity targets F1–F3 and F7 block the release or stay
   tracked goals. Needed before the release benchmark.
4. **Approved:** phase 1 first, then phase 2.
