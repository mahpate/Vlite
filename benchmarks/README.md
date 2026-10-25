# Benchmarking VLite and comparable databases

Use benchmarks to answer a workload question, not to produce a universal ranking.
Publish configuration, correctness/recall, raw measurements and limitations with
every result. [Published runs](RESULTS.md) follow this policy.

## Comparison tracks

| Track | VLite path | Suitable comparators | Scope |
|---|---|---|---|
| Embedded | Rust `Database` calls | zvec, FAISS, usearch | In-process query/index overhead; wrapper overhead reported separately |
| Service | Authenticated REST | Qdrant, Milvus, Weaviate | Client serialization, HTTP, server and query work |
| Application | Same document/embedding pipeline | Any deployment meeting the requirements | Retrieval quality, cost and end-to-end latency |

Do not put a zvec in-process query next to a VLite HTTP request and call the
difference engine performance. A product supporting different durability,
filter semantics, concurrency or scale may be the better fit despite a slower
number on a small synthetic workload.

## Native VLite benchmark

### Paired source-revision regression, October 25, 2026 UTC

The [five-repetition revision campaign](results/vector-revisions-4096-128d-wsl-quiet.json)
compares the official v0.4.1 tag (`37645fb`) with committed source `059650e`,
both default and opt-in `graph` builds. The package version remains 0.4.1;
the source revision is **not a new released version**. Uncommitted engine/server
work was excluded by building isolated `git archive` exports.

Each variant runs five fresh children for each of two workloads: 4,096 uniform
SplitMix64 vectors x 128 dimensions, 100 independent queries, squared L2,
k=10, seed 42, HNSW m=16 / construction ef=100 / search ef=100, durable
100-record ingest batches, and either zero or 16 replacements per ANN query.
Each child closes/reopens the database, verifies the count, performs one
untimed warmup per query mode, checks exact IDs/distances against an independent
full-sort oracle and independently checks mixed-query results. Exact timing
always precedes ANN; in mixed children the exact baseline is the initial state,
not a second measurement of the post-mutation state.

The [first attempt](results/vector-revisions-4096-128d-wsl.json) is explicitly
archived as contended and is excluded from the reference tables. After overlapping
work was reported, the graph session explicitly confirmed at 18:47 PDT that both
coder agents were paused and its measurement campaign was stopped. The replacement
began at 01:49:13 UTC, using the same prebuilt binaries; the session was notified
when it finished. This coordinated window is still shared hardware, not isolation.

All binaries were built first with the same Rust 1.99.1 release toolchain.
Variant order rotates, and static/mixed order alternates. There were no detected
Windows or WSL cargo/rustc processes immediately before collection. Two-second
pre-child WSL CPU samples had median 0.094%, range 0.031–3.778%; these readings do
not rule out Windows-host contention. The host is shared and unpinned:
WSL2 / Linux x86_64, Intel Xeon Platinum 8370C, 16 logical CPUs. Durability uses
the default sync-before-ack behavior on the WSL filesystem; no power-loss,
cold-machine, network or native-Windows measurement is implied.

Reproduce after exporting the two commits into separate source directories:

```sh
git archive v0.4.1 | tar -x -C /path/to/release
git archive 059650e | tar -x -C /path/to/current
git rev-parse v0.4.1 > /path/to/release/source-commit.txt
git rev-parse 059650e > /path/to/current/source-commit.txt
# In each export, use the same toolchain and separate target directories:
cargo build --locked --release --bin vlite-bench --target-dir /path/to/target
# For the third binary, build current with --features graph and a third target.
python benchmarks/revision_campaign.py \
  --variant release-0.4.1 /path/to/release /path/to/release-target/release/vlite-bench default \
  --variant main-default /path/to/current /path/to/current-target/release/vlite-bench default \
  --variant main-graph /path/to/current /path/to/graph-target/release/vlite-bench graph \
  --output new-vector-revisions.json
```

The Linux runner requires `/proc`, Python 3 and `rustc` (the recorded run used
`/root/.cargo/bin/rustc`). It creates its output exclusively, persists every
completed child and records failures instead of overwriting historical reports.
Do not build, test or run other benchmarks during timing. The graph-enabled
variant measures **vector-path overhead**, not graph traversal or graph-store
capacity; the separate graph reference campaign answers those questions.

### Mixed-load maintenance regression

`examples/maintenance_mixed_load.rs` is a seeded correctness workload with
exact/ready-ANN reads, durable mixed writes and background index maintenance.
It records raw operation latencies and tail summaries, not a pass/fail speed
threshold. The native CI matrix runs it on supported runner platforms.
The archived [Windows x86_64 development capture](results/maintenance-mixed-load-windows-x86_64.json)
records its configuration, platform, digest and raw samples. It is a debug-build,
shared-host diagnostic run, not a release benchmark or a cross-engine comparison.
Do not interpret single-process operation latency as concurrent HTTP service
latency or a guarantee that maintenance improves p99.

<a id="current-source-performance-work-october-23-2026"></a>

### Performance work included in 0.4.1, measured October 23, 2026

The optimizations below are released in v0.4.1. Measurements retain their
original development-build provenance; release availability does not turn them
into new tagged-release or cross-engine measurements.

Three bounded optimizations retain existing durability, distance precision and
index validation. Native Windows release measurements were collected on a
shared host alongside other builds; they are development observations, not
isolated production speedups or revised cross-engine results.

- **HNSW construction:** accepted unquantized neighbor vectors are borrowed once
  per selection instead of repeatedly fetched from the record tree. A paired
  reference/candidate microbenchmark of 6,400 selections over 3,000 32-dimensional
  records measured median L2 selection 127.141 -> 91.315 ms and cosine
  77.787 -> 52.580 ms. Quantized traversal retains its existing path. Reference
  equivalence and snapshot fingerprints check that selection semantics remain
  unchanged. These timings cover selection, not whole-query latency.
- **ANN snapshot codec:** batched SHA updates preserve the exact byte stream;
  numeric decoding writes into the final vector using bounded stack scratch,
  avoiding an intermediate equally sized byte vector. Across 18 samples per
  version/mode on 1,024 x 512 records plus 128 buffered updates, median save/load
  was 10.762/4.394 -> 9.591/3.957 ms without quantization and
  14.116/7.418 -> 12.818/6.915 ms with scalar u8. Snapshot hashes and search
  results matched. Database replay, owner snapshot cloning and full snapshot
  self-validation remain; total restart speedup is not established.
- **Typed embedded inputs:** opt-in C/Python float32 search and put-batch entry
  points retain JSON options/results but omit vector JSON encoding. Python
  snapshots buffers before releasing the GIL; this is not zero-copy. One paired
  128-dimensional campaign measured median query conversion 107.9 -> 43.8 us,
  search 185.5 -> 125.5 us and a durable 128-record batch 19.91 -> 7.18 ms.
  Tiny inputs can regress; existing JSON methods remain unchanged. Capability
  detection supports old native libraries with JSON fallback, or explicit
  `unsupported` when `require_native=True`.

A four-way distance-loop unroll was tested and **not retained**: its final
seven-trial median was 1,931 ns versus 1,918 ns for the scalar kernel, reversing
an earlier apparent gain. No SIMD, lower-precision accumulation or distance
speedup is claimed.

Raw samples:
[HNSW selection](results/hnsw-selection-windows.txt),
[snapshot codec](results/snapshot-codec-windows.txt),
[typed 128D](results/typed-vectors-128d-windows.json),
[typed 768D](results/typed-vectors-768d-windows.json).
Run the selection probe with
`cargo test --locked --release --lib profile_construction_and_selection -- --ignored --nocapture --test-threads=1`;
run `cargo run --locked --release --example snapshot_lifecycle_perf -- candidate`
for the fixed-workload snapshot probe (the argument labels the output), and
`python bindings/python/benchmark_typed_vectors.py --help` for typed-input
campaign options.

The initial selection/codec/Python batch did not change reader generations,
group commit or rebuild snapshot ownership. The follow-on source work below
addresses those separately. Eventual consistency and weaker durability are not
prerequisites for these improvements.

### Concurrency and Node.js capabilities released in 0.4.1

Embedded read captures and unquantized background HNSW capture share immutable
record generations instead of deep-copying vector bodies. Read captures are
limited to eight per database, each at most 100,000 records/128 MiB logical
bodies. Rebuild retention adds a 256 MiB body cap. First writes can still copy
the ID tree, and per-record shared ownership adds allocation/indirection; this
is not a packed-vector layout or lock-free persistent tree. See
[visibility, retention and publication contracts](../docs/read-snapshots.md).

The bounded 2,048 x 512 Windows experiment recorded median capture 1.932 ->
0.040 ms and warm-cache reopen 28.846 -> 7.777 ms. Reopen combines compatible
slicing-by-eight IEEE CRC validation with borrowed/reused verified-frame
buffers, while retaining full-log validation and replay. These before/after
shared-host observations are not isolated production gains. Quantized capture
remains synchronous, and there is no checkpoint or storage-format change.
Reusable construction scratch was prototyped and rejected after mixed paired
results; no scratch/layout speedup is claimed.
See [raw samples, provenance gaps and memory trade-offs](results/engine-snapshots-windows.json).
Baseline binary/source hashes were not retained, so the observed end-to-end
differences are not a fully archived A/B campaign.

```text
cargo run --release --example engine_snapshot_perf -- candidate
cargo test --release --lib replay_component_profile -- --ignored --nocapture
```

The [mixed-read/write experiment](results/engine-mixed-windows.json) compares
lock-held exact scans with snapshot scans in three paired, controlled-schedule
trials. Median durable writer latency fell from 1.221 to 0.828 ms, but reader
latency increased from 0.729 to 0.903 ms. A warmed first write with a retained
generation cost 0.124 ms versus 0.002 ms with a unique generation; maximum
snapshot-mode write latency reached 14.523 ms. These warm-cache, shared-host
results expose a trade-off, not a universal speedup or tail-latency guarantee.
Correctness checks verify pre/post-write state and exact distance bits.

```text
cargo run --release --example engine_mixed_perf
```

Opt-in server group commit keeps individual record-write recovery frames but
shares a sync across concurrently admitted requests. It does not acknowledge
before that sync. The default remains independent commits. Queues and retained
inputs are bounded, validation remains per request, and sync failure turns every
would-be success into `commit_indeterminate`. See
[configuration, route coverage and failure semantics](../docs/CONSOLE-ADMIN.md#opt-in-durable-group-commit).

Two paired Windows release runs of 512 durable writes, 16-dimensional vectors
and 128-byte payloads measured zero-delay grouping at 12,571-15,617 writes/s
versus 1,926-2,910 writes/s at concurrency 16. Sequential grouping was 6-8%
slower. An explicit 2 ms wait caused roughly 16 ms sequential latency on that
host, so the opt-in default only collects already queued work. These are noisy
shared-host, in-process observations, not network throughput or production SLOs.
Each case reopens and verifies all acknowledged records. See
[raw output and workload](results/group-commit-windows.txt).

```text
cargo test --release --features server --lib comparative_durable_write_benchmark -- --ignored --nocapture
```

Simple exact server searches can scan bounded immutable collection generations
outside the owner lock. This preserves one captured state per search, not one
snapshot across separate requests. ANN, reranking and indexed-filter paths
remain unchanged; exhausted snapshot budgets use the existing lock-held path.

The Node.js binding adds opt-in `searchF32`, `searchManyF32` and atomic
`batchPutF32`. Private input snapshots avoid vector JSON conversion; multi-query
calls amortize worker messaging, not native engine locking or transaction
boundaries. Existing APIs remain unchanged. The bounded benchmark checks record,
revision and ordered-hit parity and records native/source hashes:

```text
npm --prefix bindings/node run build
node bindings/node/benchmark-f32.mjs node-f32-results.json
```

The initial Windows sample used in-memory databases, 512 records, 384 dimensions
and 40 exact L2 queries. Its fixed variant order can favor later runs through
cache/JIT warming; no durable-ingest, zero-copy or production speedup claim is
made. See the [binding guide](../bindings/node/README.md) for bounds and fallback.
These capabilities are included in 0.4.1 packages. They do not revise historical
cross-engine tables or change the contents of older published packages.

<a id="hnsw-visited-membership-optimization-unreleased-source"></a>

### HNSW visited-membership optimization (released in 0.4.1)

Construction and filtered search use a dense visited bitmap instead of hashing
node positions on graphs up to 8,388,608 nodes. Each traversal owns its bitmap,
so concurrent readers share no mutable visited state. The bitmap uses one bit per
graph node, rounded up to 64-bit words (at most 1 MiB per traversal); larger graphs
retain the sparse hash-set path. This bounds only the bitmap, not total traversal
memory. Graph-selection rules, distance precision, filtering and durability are
unchanged.

A native Windows release run on 5,000 synthetic 128-dimensional vectors
(M=16, ef_construction=100, ef_search=100, k=10) measured median rebuild time
**5.552 s before / 5.080 s after** (8.5% lower). The mean duration of two
100-query batches was **76.392 ms / 60.795 ms** (20.4% lower).
These are batch durations, not per-query p50 or other latency percentiles. Each
campaign measured three rebuilds and two query batches (before snapshot save and
after snapshot load). Snapshot/rebuild results matched within each campaign;
these checks do not establish before/after equivalence. A separate regression
test pins the graph topology to a baseline fingerprint.

See [raw timings and limitations](results/hnsw-visited-windows.json). This is a
small warm-cache before/after experiment on a shared host, not a new zvec
comparison or evidence of a universal gain. No results were collected at
100k × 768, on other operating systems or above the sparse-fallback threshold.
Run this command separately on baseline `bb627b6` and the candidate source to
repeat the experiment (timings will vary):

```text
cargo run --quiet --release --example ann_restart_bench -- --count 5000 --dims 128 --query-count 100 --metric l2 --rebuild-trials 3
```

### Persisted ANN restart measurement

With the licensed, hash-verified GloVe subset prepared by the existing
`prepare_glove.py` workflow:

```powershell
cargo build --locked --release --example ann_restart_bench
python benchmarks\ann_restart_campaign.py --binary target\release\examples\ann_restart_bench.exe --dataset PATH_TO_PREPARED_SUBSET --output ann-restart.json
python tests\ann_restart_campaign.py
```

The campaign runs both quantization modes, checks bit-identical results over
restart and rebuild, preserves raw nanosecond samples and source/input/binary
hashes, and rejects existing output paths. Reopen/load timings include database
replay and snapshot validation. They are warm-cache observations rather than
cold-process startup or a total-memory measurement.

```text
cargo run --locked --release --bin vlite-bench -- 10000 32 100 100 10 42
cargo run --locked --release --bin vlite-bench -- --ann 16 100 100 10000 32 100 100 10 42
cargo run --locked --release --bin vlite-bench -- --filter-every 10 10000 32 100 100 10 42
cargo run --locked --release --bin vlite-bench -- --metric cosine --filter-every 100 --ann 16 100 100 2000 32 30 100 10 42
cargo run --locked --release --bin vlite-bench -- --filter-none --ann 16 100 100 2000 32 30 100 10 42
```

Measure whole-child peak memory separately using
`tests/measure_benchmark.py`. That peak includes dataset generation and the
full-sort oracle, not only query working memory. Never present input-vector
byte counts as measured process memory.

### Filter-aware ANN campaign

From the repository root, using native Windows Rust/Python:

```powershell
cargo build --locked --release --bin vlite-bench
python benchmarks\filtered_ann_campaign.py --binary target\release\vlite-bench.exe --records 2000 --dimensions 32 --queries 30 --trials 3 --ef 100 --recall-target 0.99 --output benchmarks\results\filtered-ann-synthetic-windows.json
python tests\filtered_ann_campaign.py
cargo test --locked --features server --test benchmark_cli
```

The output path must not exist. On other systems use their native executable
path. The bounded standard-library campaign runs **24 sequential fresh release
children** by default: both squared L2 and cosine across unfiltered, broad
(50%, `id % 2 == 0`), restrictive (1%, `id % 100 == 0`) and no-match predicates,
three trials each. Order reverses on alternating trials. `--filter-none` stores
`selected=false` on every record and queries `selected=true`, providing an
explicit no-match predicate without changing `--filter-every`'s existing
zero-based ID selection. Both options precede `--ann` and are mutually exclusive.
Other metric/input/mutation options precede filter options, as before.

Each child verifies exact IDs, distances and result counts against a full-sort
oracle built directly from the same vectors and predicate, outside timing.
The same filter goes to exact, lazy ANN and ready-only ANN APIs; no exact
fallback is used. Initial explicit index construction and one warmup per mode
are outside measured query samples. Static queries use the ready-only ANN path;
mixed lazy mode can rebuild an unavailable graph synchronously, while background
and maintenance modes retain their explicit ready-only rejections.
Mixed writes preserve the synthetic metadata and compare to the current live,
eligible exact oracle.

Top-level `eligible_records` is the initial eligible dataset count (excluding
zero-norm vectors for cosine), `returned_per_query` is its capped count, and
`returned_counts` records actual exact counts per query. `hnsw` records
`eligible_records_samples`, `returned_counts`, `recall_denominator_samples`
and summed `recall_denominator` for each current query state. Cosine excludes
zero-norm stored vectors and rejects zero query vectors. Recall is tie-aware against the kth **eligible** exact
distance, not global top-k; the denominator is `min(k, eligible exact count)`
per query. Existing empty-set convention is preserved: recall **1 with explicit
denominator 0**, a vacuous result, not evidence of retrieval quality. The
campaign's empty-set recall-target verdict is null. Nonempty target checks use
measured trial recall; choosing an ef does not establish a recall guarantee.

Filtering participates in candidate retention while nonmatching nodes remain
traversal bridges. An ef bounds retained eligible candidates, **not visits or
time**. Restrictive/no-match filters may traverse the entire reachable graph,
and ANN does not promise to fill k. Compare against exact latency rather than
assuming filtering makes ANN faster. Reports retain every raw exact/ANN latency
sample, build cost, returned counts, recall denominators, workload checksums,
source/binary hashes and OS-measured whole-child peak RSS via
`tests\measure_benchmark.py`. RSS includes input/oracle/index/all phases, not
incremental index memory. Synthetic data, shared unpinned host, small query
counts and single-client warm-cache timing do not establish production tails,
real-embedding quality or a universal speedup.

### Indexed and quantized search comparison

```powershell
cargo build --locked --release --example search_capabilities_bench
python benchmarks\search_capabilities_campaign.py --binary target\release\examples\search_capabilities_bench.exe --output search-capabilities.json
python tests\search_capabilities_campaign.py
```

This bounded example compares unquantized and scalar-u8 ANN on 2,000 clustered
32-dimensional synthetic vectors, both squared L2 and cosine, against exact
top-k. It verifies exact returned distances outside timed queries and retains
every query sample, denominator, component byte count and source/binary hash.
A separate selective metadata query must preserve exact results while scanning
fewer records after index declaration. No hidden ANN-to-exact fallback is used.

Code/codebook and graph-link counts are component accounting, not OS-measured
RSS. Original f32 records remain resident: this implementation can add memory
for quantization, so do not advertise total-memory savings. One shared-host run,
fixed benchmark order and synthetic clusters do not establish production recall
or tail latency. The campaign rejects an existing output path and validates its
result shape before writing a successful report.

## Reproduce the REST comparison

`compare_rest.py` uses only Python's standard library and Docker. It creates
unique benchmark containers/volumes, runs engines **sequentially**, and stops
and removes only its own containers. All database volumes are retained and their
names recorded; there is no destructive automatic volume cleanup.

Build the VLite release image and pull the pinned comparison version:

```text
docker build --target runtime -t vlite-perf:local .
docker pull qdrant/qdrant:v1.14.1
python3 benchmarks/compare_rest.py --vlite-image vlite-perf:local --mode exact --records 10000 --dimensions 32 --queries 100 --k 10 --trials 3 --output exact.json
python3 benchmarks/compare_rest.py --vlite-image vlite-perf:local --mode exact --filter-every 10 --records 10000 --dimensions 32 --queries 100 --k 10 --trials 3 --output filtered.json
python3 benchmarks/compare_rest.py --vlite-image vlite-perf:local --mode ann --records 10000 --dimensions 32 --queries 100 --k 10 --trials 3 --output ann.json
```

Run from the repository root; output files must not exist. WSL users run inside
WSL. Both containers receive the same default **2 CPU / 2 GiB limit**, loopback
publication, separate local named volumes, and independently generated API
secrets. Each trial creates fresh data; engine order alternates between trials.
Use immutable image digests for archival runs. JSON records actual image IDs,
git commit/dirty state, Python/kernel/Docker resources, workload seed/checksum,
all latency samples, recall, version information, retained volumes and errors
(`status=failed` is never a successful report).

### What is measured

- Deterministic SplitMix64 fixed-point f32 vectors, independent seeded queries.
  The SHA-256 covers dataset **and** queries. No embedding model or real-data
  representativeness is implied.
- Squared L2 is the comparison metric. Qdrant's Euclidean scores are squared
  by the adapter and checked against the same f64 oracle with a stated floating
  tolerance (2e-5 relative / 1e-5 absolute).
- Exact ground truth is computed directly from input vectors outside timing.
  Ties at the kth exact distance are equally correct, accommodating different
  ID tie policies. Exact results must have the full eligible result count and
  recall 1.0. Every returned ID must be valid/unique/eligible.
- Both engines store the same boolean `selected` field. Exact filtered runs use
  a 1/N selection rate. Neither adapter creates a scalar index in this track;
  indexed filtering should be a separate declared experiment.
- HNSW construction m=16, ef-construction=100, search ef=100; no quantization.
  These matching parameter names do **not** imply identical implementations.
  Qdrant ANN indexing must finish before timing (indexed vector count checked);
  ANN sets `full_scan_threshold=10` (Qdrant 1.14.1's minimum, in KiB) to keep this workload from silently
  using Qdrant's default scan-preferred planner. Exact mode retains its default
  threshold and explicitly requests exact search.
  VLite builds synchronously on the initial query. Index wait/first-query
  latency is reported separately from steady-state warm queries.
- Ten warmups (or the query count if smaller); persistent HTTP connections;
  one client, concurrency=1; no TLS in the localhost benchmark.
  Both APIs authenticate requests. Client serialization, response transfer,
  parsing and Qdrant score conversion are included.
- Nearest-rank p50/p95/p99 per trial and sequential queries/second. This is
  **not maximum sustainable server QPS**. Repeated queries share warm cache.
  Do not pool all trials and hide run-to-run variation.

### What is not equivalent or measured

VLite synchronizes each batch before acknowledgment. Qdrant uses
`wait=true`, which waits for update application but is **not a claim of the
same filesystem/WAL flush contract**. Ingestion time is diagnostic, not a fair
durability/throughput ranking. Background optimizers and logging differ.
Container CPU limits do not pin physical cores or prevent host contention.
Neither peak process memory nor disk footprint, cold starts, crash/power-loss
behavior, concurrent writes, replication or high availability are measured by
this comparison. Memory is explicitly null, not estimated.

Use 3+ repetitions on a dedicated host for publishable comparisons. Sweep:
record count (10k/100k/1m within tool bounds or a separately reviewed runner),
dimensions (32/384/768/1536), k, selectivity, search ef, concurrency and mutation
rate. Compare ANN latency **at a common measured recall target**, not merely at
equal ef. Preserve underperforming runs, tail latency and indexing costs.
This bounded standard-library harness rejects more than 100 million oracle
distance components; large-scale work needs an efficient validated oracle.

## Embedded comparison with zvec

`compare_zvec.py` compares the VLite Python embedded package with the
[zvec](https://zvec.org/en/) Python package in-process (Linux; `/proc` is used
for memory). Both engines see the same seeded dataset, the same ingest batch
size, L2 distance and the same HNSW parameters (`m=16`, `ef_construction=100`,
`ef=100`; zvec's larger defaults are overridden). Each (trial, mode, engine)
runs in a fresh process and the engine order alternates between trials.

```text
python -m venv .venv && . .venv/bin/activate
pip install zvec numpy <vlite_embedded-0.3.0-...-manylinux_2_28_x86_64.whl>
python benchmarks/compare_zvec.py --records 20000 --dimensions 128 --trials 3 \
  --output benchmarks/results/zvec-20k-128d-wsl.json
python benchmarks/compare_zvec.py --records 100000 --dimensions 768 --trials 2 \
  --output benchmarks/results/zvec-100k-768d-wsl.json
```

It reports ingest, index build (`build_index()` / `optimize()`), per-query
p50/p95/p99, sequential QPS, tie-aware recall@k against a float64 oracle,
resident-memory growth, on-disk footprint and reopen-plus-first-query time.
VLite fsyncs each batch as an atomic commit and keeps HNSW in memory, so its
ANN reopen includes a full index rebuild; zvec persists its index. When the
installed VLite library exports the float32 entry points, the harness ingests
with `batch_put_f32` and searches with `search_f32` (`approximate_ready` for
ANN), recording `vector_transport: "float32"`; otherwise it sends each vector as
JSON through the C ABI (`"json"`), which adds wrapper overhead that a Rust caller
would not pay. Results committed before this field existed used JSON. Published
results and caveats are on the
[comparison page](https://mahpate.github.io/Vlite/compare.html).

For other engines, pin the package and index parameters, use the same
vectors/IDs and metric, and check recall against the same oracle before
trusting timings.

FAISS is an index library, not a durable vector database; durability/CRUD
comparisons require an application storage layer. For Milvus/Weaviate, record
all supporting services and total resource allocation, not just the query node.
Do not borrow marketing numbers from upstream benchmark pages for our table.
Only engines whose adapters have actually run here (see the multi-engine
comparison below, plus Qdrant over REST and zvec embedded) have published
numbers.

## Multi-engine comparison

`compare_engines.py` runs VLite next to other popular vector databases on the
same seeded data, queries and float64 ground truth, in two tiers that are
**never ranked together**:

| Tier | Engines | Transport |
|---|---|---|
| `--tier embedded` (in-process Python) | VLite embedded, zvec, Chroma `PersistentClient`, LanceDB, FAISS | Python calls in the benchmark process |
| `--tier server` (client/server on localhost) | VLite server, Qdrant (REST), Qdrant (gRPC, extra row), PostgreSQL + pgvector | One persistent connection from the host to the container's bridge IP |

FAISS is an **in-memory library baseline**: no persistence, durability or
reopen (those columns are N/A). It shows the cost of an index with no storage
engine around it, not a database to choose instead.

```text
# Linux/WSL with Docker; one venv
pip install numpy zvec chromadb lancedb faiss-cpu "psycopg[binary]" pgvector qdrant-client \
  <vlite_embedded-0.3.0-...-manylinux_2_28_x86_64.whl>
docker pull qdrant/qdrant:v1.15.5 && docker pull pgvector/pgvector:0.8.1-pg17 \
  && docker pull ghcr.io/mahpate/vlite-api:v0.3.0
python benchmarks/compare_engines.py --tier embedded --records 20000 --dimensions 128 --trials 3 \
  --output benchmarks/results/engines-embedded-20k-128d-wsl.json
python benchmarks/compare_engines.py --tier server --records 20000 --dimensions 128 --trials 3 \
  --output benchmarks/results/engines-server-20k-128d-wsl.json
# 100k x 768d: same commands with --records 100000 --dimensions 768 --trials 2
#   --output benchmarks/results/engines-{embedded,server}-100k-768d-wsl.json
python benchmarks/compare_engines.py --summary benchmarks/results/engines-*.json   # median tables
```

`--engines vlite,faiss` and `--modes ann` select subsets; `--cpus`/`--memory`
(default 4 CPUs / 4 GiB) apply identically to every server container;
`--build-timeout` (default 1200 s) records slower builds as `timeout`.

### Methodology

- **Data:** clustered Gaussian float32 vectors and queries (`--clusters 64
  --spread 0.35 --seed 42`, 200 queries, k=10); the dataset SHA-256 is in the
  JSON. Recall@10 is ID-based and tie-aware against a float64 NumPy oracle
  computed outside timing. Squared L2 everywhere (Qdrant `Euclid` and Chroma
  `l2` rank identically).
- **Isolation:** each (engine, mode, trial) runs in a fresh child process and,
  for servers, a fresh container and volume; engine order rotates per trial.
- **Ingest:** identical batch size for every engine (`max(1, min(1000,
  4_000_000 // (dims*24)))`, i.e. 1000 at 128d, 217 at 768d), one commit per
  batch: VLite `batch()` / `POST /v1/batches`, zvec `insert`, Chroma `add`,
  LanceDB `add`, FAISS `add`, Qdrant upsert `wait=true`, pgvector `COPY BINARY`
  in one transaction per batch.
- **Exact:** brute force — VLite `search`, zvec flat index, LanceDB
  `bypass_vector_index`, FAISS `IndexFlatL2`, Qdrant `exact=true` (no HNSW
  built), pgvector sequential scan. Chroma has no exact mode (N/A).
- **ANN:** HNSW with VLite's defaults `M=16, ef_construction=100` set
  explicitly on every engine that exposes them, `ef_search=100`. LanceDB has no
  plain HNSW, so it uses `IVF_HNSW_SQ` with one partition (8-bit scalar
  quantized, no refine), which explains its lower recall. Qdrant bulk-loads with
  `indexing_threshold=0` and the build step enables indexing and waits for
  `green` with all vectors indexed (`full_scan_threshold=10` so HNSW serves
  queries). pgvector builds `CREATE INDEX ... USING hnsw` after ingest. Chroma
  and FAISS insert into the graph during `add`, so their graph cost is in
  *ingest*; compare **ingest+build** across engines.
- **Latency:** sequential, one client thread, one query in flight, after 10
  warmup queries; QPS = queries / sum(latency). This is not a throughput or
  concurrency benchmark.
- **Memory:** embedded = process VmRSS growth from "empty database opened" to
  "index built"; server = container memory (`docker stats`) growth over the
  same span. **Disk:** bytes after clean close / container stop. **Reopen:**
  reopen (or `docker start`) + readiness + any required index rebuild + first
  query.
- **Server containers:** the same `--cpus 4 --memory 4g` for all three images.
  Postgres gets `shared_buffers=1GB`, `maintenance_work_mem=1GB` (standard
  pgvector build tuning) and `--shm-size 1g`. Clients connect to the container
  IP because Docker's userland port proxy added ~0.5–1 ms per round trip in
  smoke tests (`--published-ports` restores the proxy path).

### Durability defaults (not changed; differ between engines)

| Engine | Default durability per acknowledged write |
|---|---|
| VLite (embedded and server) | Each batch is one atomic commit, fsynced before acknowledgment |
| zvec | Defaults; harness calls `flush()` before close; per-insert fsync not verified |
| Chroma | SQLite WAL; HNSW segment files persisted asynchronously in batches |
| LanceDB | Each `add` commits a new table version; per-commit fsync not verified |
| FAISS | None (in-memory only) |
| Qdrant | WAL appended per request; WAL/segments flushed every 5 s, not fsync per request |
| pgvector | `fsync=on`, `synchronous_commit=on`: WAL fsync at each batch commit |

VLite pays an fsync for every batch while some engines do not; that is part of
its ingest time.

### Results: 20,000 x 128d (WSL2, 16 vCPU Xeon Platinum 8370C, medians)

Embedded tier, 3 trials
([engines-embedded-20k-128d-wsl.json](results/engines-embedded-20k-128d-wsl.json)):

| mode | engine | p50 ms | p95 ms | QPS | recall@10 | ingest s | build s | ingest+build s | RSS growth MiB | disk MiB | reopen+1st query s |
|---|---|---|---|---|---|---|---|---|---|---|---|
| exact | vlite 0.3.0 | 1.413 | 1.553 | 695 | 1.0000 | 1.80 | 0.00 | 1.80 | 30.8 | 10.2 | 0.07 |
| exact | zvec 0.8.0 | 1.153 | 1.482 | 842 | 1.0000 | 0.33 | 0.03 | 0.36 | 49.8 | 13.7 | 0.02 |
| exact | chroma 1.5.9 | N/A | N/A | N/A | N/A | N/A | N/A | N/A | N/A | N/A | N/A |
| exact | lancedb 0.40.0 | 10.525 | 12.083 | 92 | 1.0000 | 0.19 | 0.00 | 0.19 | 130.8 | 9.8 | 0.01 |
| exact | faiss 1.15.1 (library) | 0.251 | 0.701 | 2,739 | 1.0000 | 0.01 | 0.00 | 0.01 | 25.1 | N/A | N/A |
| ann | vlite 0.3.0 | 0.454 | 0.556 | 2,154 | 1.0000 | 1.74 | 11.99 | 13.73 | 31.0 | 10.2 | 12.08 |
| ann | zvec 0.8.0 | 0.438 | 0.559 | 2,212 | 1.0000 | 0.31 | 0.35 | 0.66 | 65.9 | 15.4 | 0.03 |
| ann | chroma 1.5.9 | 1.672 | 1.886 | 591 | 1.0000 | 5.65 | 0.00 | 5.65 | 94.4 | 16.5 | 0.08 |
| ann | lancedb 0.40.0 (IVF_HNSW_SQ) | 7.188 | 8.279 | 133 | 0.9485 | 0.19 | 0.93 | 1.12 | 228.5 | 15.4 | 0.03 |
| ann | faiss 1.15.1 (library) | 0.079 | 0.147 | 11,339 | 0.9995 | 2.52 | 0.00 | 2.52 | 31.1 | N/A | N/A |

Server tier, 3 trials, 4 CPUs / 4 GiB per container
([engines-server-20k-128d-wsl.json](results/engines-server-20k-128d-wsl.json)):

| mode | engine | p50 ms | p95 ms | QPS | recall@10 | ingest s | build s | ingest+build s | memory growth MiB | disk MiB | restart+1st query s |
|---|---|---|---|---|---|---|---|---|---|---|---|
| exact | vlite-server v0.3.0 (REST) | 2.061 | 2.328 | 481 | 1.0000 | 1.87 | 0.00 | 1.87 | 26.6 | 10.1 | 1.32 |
| exact | qdrant 1.15.5 (REST) | 1.413 | 1.621 | 699 | 1.0000 | 1.85 | 0.00 | 1.85 | 66.7 | 196.5 | 1.62 |
| exact | qdrant 1.15.5 (gRPC) | 1.926 | 2.270 | 512 | 1.0000 | 0.70 | 0.00 | 0.70 | 19.5 | 196.1 | 1.79 |
| exact | pgvector 0.8.1 / PG 17.8 | 4.189 | 4.719 | 234 | 1.0000 | 0.23 | 0.04 | 0.26 | 13.2 | 66.2 | 1.41 |
| ann | vlite-server v0.3.0 (REST) | 1.149 | 1.356 | 845 | 1.0000 | 1.82 | 12.25 | 14.06 | 29.0 | 10.1 | 13.96 |
| ann | qdrant 1.15.5 (REST) | 0.965 | 1.127 | 1,027 | 1.0000 | 1.89 | 0.93 | 2.82 | 92.7 | 165.3 | 1.63 |
| ann | qdrant 1.15.5 (gRPC) | 1.464 | 1.641 | 681 | 1.0000 | 0.70 | 0.93 | 1.63 | 60.6 | 165.3 | 1.67 |
| ann | pgvector 0.8.1 / PG 17.8 | 1.016 | 1.236 | 961 | 1.0000 | 0.23 | 2.72 | 2.95 | 32.1 | 98.1 | 1.40 |

### Results: 100,000 x 768d (same host, medians of 2 trials, batch 217)

Embedded tier
([engines-embedded-100k-768d-wsl.json](results/engines-embedded-100k-768d-wsl.json)):

| mode | engine | p50 ms | p95 ms | QPS | recall@10 | ingest s | build s | ingest+build s | RSS growth MiB | disk MiB | reopen+1st query s |
|---|---|---|---|---|---|---|---|---|---|---|---|
| exact | vlite 0.3.0 | 32.210 | 34.627 | 31 | 1.0000 | 50.62 | 0.00 | 50.62 | 333.6 | 295.3 | 1.89 |
| exact | zvec 0.8.0 | 27.033 | 29.036 | 37 | 1.0000 | 5.00 | 0.53 | 5.53 | 346.6 | 300.9 | 0.11 |
| exact | chroma 1.5.9 | N/A | N/A | N/A | N/A | N/A | N/A | N/A | N/A | N/A | N/A |
| exact | lancedb 0.40.0 | 70.423 | 85.269 | 14 | 1.0000 | 3.63 | 0.00 | 3.63 | 193.2 | 302.6 | 0.07 |
| exact | faiss 1.15.1 (library) | 21.585 | 33.797 | 48 | 1.0000 | 0.37 | 0.00 | 0.37 | 307.5 | N/A | N/A |
| ann | vlite 0.3.0 | 3.545 | 3.936 | 282 | 0.9680 | 51.49 | 524.57 | 576.06 | 354.3 | 295.3 | 518.75 |
| ann | zvec 0.8.0 | 0.901 | 1.194 | 1,075 | 0.9657 | 4.67 | 7.75 | 12.42 | 375.3 | 314.7 | 0.04 |
| ann | chroma 1.5.9 | 2.474 | 2.837 | 397 | 0.9692 | 54.14 | 0.00 | 54.14 | 427.2 | 326.7 | 0.68 |
| ann | lancedb 0.40.0 (IVF_HNSW_SQ) | 10.160 | 12.602 | 96 | 0.9055 | 3.69 | 16.58 | 20.27 | 554.4 | 391.8 | 0.30 |
| ann | faiss 1.15.1 (library) | 0.505 | 0.597 | 1,938 | 0.9550 | 53.18 | 0.00 | 53.18 | 343.5 | N/A | N/A |

Server tier, 4 CPUs / 4 GiB per container
([engines-server-100k-768d-wsl.json](results/engines-server-100k-768d-wsl.json)):

| mode | engine | p50 ms | p95 ms | QPS | recall@10 | ingest s | build s | ingest+build s | memory growth MiB | disk MiB | restart+1st query s |
|---|---|---|---|---|---|---|---|---|---|---|---|
| exact | vlite-server v0.3.0 (REST) | 34.022 | 37.257 | 29 | 1.0000 | 56.37 | 0.00 | 56.37 | 339.9 | 294.5 | 3.16 |
| exact | qdrant 1.15.5 (REST) | 16.710 | 20.066 | 58 | 1.0000 | 55.16 | 0.00 | 55.17 | 39.5 | 486.1 | 2.03 |
| exact | qdrant 1.15.5 (gRPC) | 17.859 | 22.309 | 55 | 1.0000 | 18.08 | 0.00 | 18.08 | 56.2 | 486.1 | 1.92 |
| exact | pgvector 0.8.1 / PG 17.8 | 320.618 | 381.030 | 3 | 1.0000 | 8.00 | 0.03 | 8.03 | 428.4 | 777.5 | 2.19 |
| ann | vlite-server v0.3.0 (REST) | 4.942 | 8.215 | 198 | 0.9680 | 59.49 | 637.08 | 696.57 | 369.5 | 294.5 | 605.54 |
| ann | qdrant 1.15.5 (REST) | 1.957 | 2.307 | 501 | 0.9693 | 73.85 | 21.21 | 95.06 | 370.9 | 491.8 | 1.70 |
| ann | qdrant 1.15.5 (gRPC) | 2.465 | 2.882 | 405 | 0.9750 | 13.22 | 18.85 | 32.07 | 374.6 | 491.8 | 1.84 |
| ann | pgvector 0.8.1 / PG 17.8 | 2.797 | 3.379 | 351 | 0.9753 | 5.49 | 78.97 | 84.46 | 857.5 | 1488.2 | 1.60 |

Two trial-0 cells (Qdrant gRPC exact, Qdrant REST ANN) first failed because the
harness reused a keep-alive REST socket that Qdrant had closed while idle; the
harness now reconnects for idempotent requests and those two cells were re-run
alone afterwards (marked `rerun_note` in the JSON). The two VLite server ANN
trials differ widely (build 518 s vs 756 s, p50 4.2 vs 5.7 ms), so treat that
row as noisy. pgvector's unindexed exact scan reads TOASTed 3 KiB vectors with
default parallelism (`relation_bytes` 403 MiB exact / 794 MiB with HNSW).

At 768d the gap in **index build** dominates: VLite needs ~9–12 minutes for
100k vectors (and the same again on every reopen/restart, since the graph is
not persisted here) where zvec takes 8 s, LanceDB 17 s, Qdrant ~20 s and
pgvector ~80 s. VLite's ANN recall (0.968) is in line with the other HNSW
engines at the same `M/ef` (0.955–0.978), but its p50 is 2–4x zvec/FAISS in
process and ~2x Qdrant/pgvector over the network. Exact scans are comparable
in process (VLite 32 ms vs zvec 27 ms, FAISS 22 ms) but Qdrant's server-side
scan is ~2x faster than VLite server's. VLite's ingest (~50 s) is ~10x zvec /
LanceDB and close to Chroma, FAISS-HNSW and Qdrant REST, mostly JSON encoding
plus one fsync per 217-vector batch. VLite keeps the smallest disk footprint
of the persistent databases.

### Reading these results

- **Query latency.** VLite's HNSW query latency matches zvec in-process and is
  within ~0.2 ms of Qdrant/pgvector over the network at 100% recall. Its exact
  scan is slower than zvec/FAISS (the Python binding marshals JSON through a C
  ABI) and, as a server, slower than Qdrant's multi-segment parallel scan.
  FAISS is fastest because it is a bare in-memory index.
- **Index build is VLite's clear weakness** at this release: 12 s for 20k x
  128d versus 0.35 s (zvec), ~1 s (Qdrant, LanceDB) and 2.7 s (pgvector).
  Because the graph is in memory only, VLite's ANN reopen also rebuilds it,
  while the other databases persist their index and reopen in milliseconds
  (embedded) or ~1.5 s (container start). Opt-in HNSW snapshots (see
  [Persisted ANN restart measurement](#persisted-ann-restart-measurement)) are
  not used here.
- **Ingest.** VLite fsyncs every batch; Qdrant and Chroma do not, pgvector
  fsyncs WAL per commit but uses binary `COPY`. Qdrant REST ingest equals VLite
  REST; gRPC halves it (JSON encoding cost on both REST rows).
- **Footprint.** VLite has the smallest disk and memory growth of the
  databases. Qdrant's ~165–196 MiB is mostly preallocated WAL segments; the
  pgvector volume includes ~40 MiB of cluster catalog/WAL — the `items` table
  plus index itself is 11.6 MiB (exact) / 27.5 MiB (ANN), recorded as
  `relation_bytes`.
- **Recall.** All engines but LanceDB reach ≥0.9995 at `ef_search=100` on this
  easy clustered set; LanceDB's 0.9485 reflects its quantized
  `IVF_HNSW_SQ` index without refine.

Caveats: one host, synthetic data, WSL2 virtualization, small datasets that
fit in cache, single-threaded sequential queries, Python client overhead in
every row (and VLite embedded crosses a JSON C ABI). Server latency includes
the localhost round trip: in a 2,000-vector smoke run every server engine
measured ~0.9–1.0 ms p50, so most of the server p50 here is transport. Use the
raw JSON (per-query
samples, versions, params, durability notes, container image IDs) before
drawing conclusions.

## Native profiling, real vectors and mixed updates

Build `cargo build --locked --release --bin vlite-bench`, then run from a source
checkout (the campaign records Git provenance):

```powershell
python benchmarks\native_campaign.py --binary target\release\vlite-bench.exe --records 1024 --dimensions 384 --queries 30 --trials 3 --ef 32 100 --mixed-writes 16 --recall-target 0.99 --output native-384.json
```

On Linux/macOS use `python3` and `target/release/vlite-bench`. Each independent
release child creates and removes its own database. Static and mixed modes
alternate order between trials; ef values run in the listed order. The report
preserves every child's raw samples, source hashes/dirty state, binary SHA-256,
host information, and per-trial summary. Output files are never overwritten.
Failed children produce `status=failed`, not a successful partial campaign.
CPU/memory are **not** container-limited or pinned in this native track.

Each child measures ingestion, reopen, initial exact query latency, explicit
HNSW build time, and ANN query latency. Mixed mode commits a batch of N
replacement vectors before **each** ANN query, cycling existing IDs; vectors
come from the seeded generator, not the queries. Write latency includes durable
batch sync and is recorded separately. ANN latency includes any synchronous
rebuild triggered by buffer overflow. Independent ground truth and an exact
engine query check the current post-write dataset outside ANN timing.
This is sequential update/query interleaving, **not concurrent readers/writers**,
and does not benchmark deletion, insertion, or the REST server.

ANN recall is tie-aware at the exact kth distance. The declared recall target
is satisfied only if every trial's mean recall meets it. Different workloads
cannot be ranked together even if both satisfy the target: buffered exact
scanning can raise mixed-workload recall. No setting is selected by latency
alone. With only 30 queries, nearest-rank p99 is the maximum sample; this exposes
rebuild pauses but is not a stable production tail estimate.

`process_peak_rss_bytes` is an OS-measured **whole child** high-water mark,
including generation, dataset, database, exact oracle, graph, mutation buffer
and all phases. It is not incremental graph RAM or query-only memory.
`file_bytes` and `final_file_bytes` are measured logical database lengths,
not allocated filesystem blocks or total storage cost. Raw write/query samples
allow phase-level diagnosis; these are **not CPU stack profiles** of traversal,
distance functions or allocation. Use a platform CPU profiler before making
low-level optimization claims.

For licensed/public or user-owned embeddings, export two UTF-8 text files:
one vector per line, whitespace-separated finite f32 components, no header,
IDs or blank lines. Dataset/query row counts and dimensions must exactly match
the positional arguments. Inputs are bounded to 64 MiB per text file (and at
most 32 bytes per expected component plus row separators); existing decoded
vector/oracle bounds still apply. Input is validated before database creation.
Keep original input-file SHA-256, model/version, normalization, license and
train/query split with your experiment; VLite reports decoded dataset/query
FNV-1a checksums for continuity, not cryptographic provenance.

```powershell
target\release\vlite-bench.exe --vectors documents.txt --query-vectors queries.txt --ann 16 100 100 1000 384 100 100 10 42
```

External-input flags and `--mixed-writes` precede `--filter-every`/`--ann`.
Do not publish private embeddings. `native_campaign.py` and `metric_campaign.py`
use synthetic data; the separate real-data campaign below records its source.
Large datasets require a separately validated faster oracle;
the bounded native tool is not a million-vector production load generator.

## Real GloVe embeddings and mixed deletions

This harness and its CLI flags are included in 0.4.1 (added after v0.1.2).
Build the benchmark from this checkout; the existing v0.1.2 release binary
does not contain them. No engine/SDK API or storage format changes are required.

`prepare_glove.py` extracts a bounded subset of
[ANN-Benchmarks GloVe 100D](https://ann-benchmarks.com/glove-100-angular.hdf5).
The upstream [conversion](https://github.com/erikbern/ann-benchmarks/blob/main/ann_benchmarks/datasets.py)
loads Stanford's GloVe Twitter 27B word vectors and splits train/test with
`random_state=1`. Stanford's [primary distribution page](https://nlp.stanford.edu/projects/glove/)
explicitly licenses **pretrained vector data** under
[PDDL v1.0](https://opendatacommons.org/licenses/pddl/1-0/);
the separate Apache license applies to its code. Cite Pennington, Socher and
Manning, *GloVe: Global Vectors for Word Representation*, EMNLP 2014.
No source embeddings or HDF5 files are committed here.

Download and prepare outside the checkout. Extraction alone needs optional
`h5py`/`numpy`; the campaign itself uses only Python's standard library:

```powershell
python -m venv C:\benchdata\embedding-env
C:\benchdata\embedding-env\Scripts\python.exe -m pip install -r benchmarks\requirements-embeddings.txt
curl.exe --fail --location --output C:\benchdata\glove-100-angular.hdf5 https://ann-benchmarks.com/glove-100-angular.hdf5
C:\benchdata\embedding-env\Scripts\python.exe benchmarks\prepare_glove.py --source C:\benchdata\glove-100-angular.hdf5 --output C:\benchdata\glove-subset
cargo build --locked --release --bin vlite-bench
python benchmarks\embedding_campaign.py --binary target\release\vlite-bench.exe --dataset C:\benchdata\glove-subset --output glove-results.json
```

Create `C:\benchdata` first. On Linux/macOS use an equivalent external directory,
`python3`, the virtualenv's `bin/python` and `target/release/vlite-bench`.
The verified HDF5 SHA-256 is
`544af1d5e84e112cd4749571dcfd8ca109818a572f850af75a3a09e093a953c4`;
preparation refuses a different source rather than silently trusting a mutable URL.
Output directory/report must not exist; failures are explicit and partial exports
must not be used as complete datasets. Review new upstream data before changing
the pin. The extraction manifest includes source and exact text/decoded-f32
SHA-256s, shapes, source row ranges, library versions, license and model provenance.
Casting to f32 is explicit; 9-digit text is checked for bit-exact f32 round trips.
No normalization, zero-vector filtering, or query augmentation is performed.

Defaults: 3,000 base rows from upstream `train[0:3000]`, 100 queries from
`test[0:100]`, and 1,600 replacements from disjoint `train[3000:4600]`.
These are prefixes of upstream's shuffled splits, not a newly randomized sample;
source-row separation does not guarantee distinct word meanings or vector values.
Ground truth is recomputed for this **subset**, never borrowed from upstream's
full-million neighbor labels.

The campaign executes exact, 10%-eligible filtered exact, and cosine ANN
ef=32/100/300 in static and mixed modes, three trials each (24 fresh children).
Trial order reverses on alternate trials; graph seed and inputs remain fixed.
Construction uses m=16/ef=100. Every mixed query follows one durable batch of
16 real replacements and 4 deletes. IDs advance by 20 modulo the initial
record count: replacements occupy the first 16 slots, deletes the next 4.
Later cycles may reinsert deleted IDs; the published defaults stop before a
complete cycle and finish with 2,600 live records. These are sequential mutations,
not concurrent readers/writers. Mixed timings cannot be ranked as static timings.

The native CLI adds `--update-vectors <file>` and `--mixed-deletes <N>` before
`--ann`; update text has exactly `queries * mixed-writes` rows. External mixed
writes **require** that file, so real-data runs cannot silently replace vectors
with synthetic values. Synthetic runs retain their previous generator.
At most 256 total mutations/query, no more than the base record count, and
65,536 mutations/run are allowed; existing memory/oracle limits also apply.
Live counts, per-query recall, update checksums and one-based rebuild query
numbers accompany raw latency/write samples. A ready-only query is timed;
on `IndexNotReady` the same timer includes the lazy synchronous rebuild.
There is no untimed traversal probe. Deleted IDs must not appear, and exact
search plus an independent live-state full-sort oracle are checked after each
mutation batch, including empty eligible sets (recall 1 by convention).

The report records source/binary hashes and dirty state, full dataset manifest,
raw measurements, whole-child peak RSS and per-trial summaries. A 99% target
means **every trial's mean recall**, not 99% for every query. Per-query values
remain visible. Rebuild tails are not excluded from p95/p99; the separate
non-rebuild median is diagnostic only. Reports must finish with `status=complete`.
This is a small 100D word-embedding benchmark on a shared host, not a modern
sentence-embedding, million-vector, crash-durability or production SLA result.

## Background rebuild latency and availability

Build the current source, then reuse the pinned GloVe subset above:

```powershell
python benchmarks\background_campaign.py --binary target\release\vlite-bench.exe --dataset C:\benchdata\glove-subset --output background-results.json
```

The paired campaign runs lazy synchronous and background ready-only modes
at zero and 250-ms inter-request sleeps, three trials each (12 fresh children).
Modes run sequentially, reversing order on alternate trials. Inputs, mutations,
metric, m=16/ef-construction=100/ef-search=100 and k=10 match. Both modes build
their initial graph synchronously before measured mutation/query requests.
Sleep is **before each request**, not a fixed arrival-rate generator.
Sleep and independent oracle work are excluded from request timing, but both
let workers progress between requests; no production-throughput claim follows.

Background policy: poll once before each mutation/query; on Idle, Published
or CatchUpOverflow immediately start another bounded snapshot build. No other
thread reads/writes the database, and there is at most one worker. A missing
matching index yields a recorded ready-only rejection, **not** exact or lazy
fallback. Terminal overflow and publication query numbers remain in the raw
report; the final in-flight job is cancelled/joined during close.

Always pair latency with availability. The report separates served query
latency/recall from attempted queries, records every `IndexNotReady`, and counts
rejected attempts as zero recall in the aggregate delivered-recall number.
An empty eligible set still uses the native recall-1 convention.
The all-attempt native `query_p50_ms` may include quick rejections; it is
**not a successful-query latency statistic**. Summary served p50/p99 explicitly
exclude rejections. Means/percentiles remain per trial, not pooled across trials.
No-served-query summaries are null, not successful-looking defaults.

`request_samples_ms` includes poll/publication, snapshot cloning when starting,
mutation generation, durable batch sync, owner live-map updates and the ready/lazy
query. Those owner costs must not be hidden behind a query-only improvement.
`maintenance_samples_ms` isolates poll/start overhead. Worker construction
overlaps requests and uses CPU; whole-child peak RSS includes old graph,
snapshot, replacement, catch-up originals, database and all benchmark phases.
Record/vector limits are **not** total RSS limits. Worker cancellation/close
are not request latency. This experiment tests an explicitly selected Rust
API/policy, not automatic background behavior in REST or embedded wrappers.
Use [the lifecycle contract](../docs/REFERENCE.md#background-hnsw-rebuilds-rust)
for failure, overload, shutdown and version compatibility details.

### Proactive maintenance experiment

The same runner accepts `--modes lazy background maintenance` (default remains
`lazy background`) and up to three distinct `--interval-ms` values in 0..1000.
For the archived paired high-water experiment:

```powershell
python benchmarks\background_campaign.py --binary target\release\vlite-bench.exe --dataset C:\benchdata\glove-subset --output maintenance-results.json --modes lazy maintenance --interval-ms 0 250 1000 --trials 3
```

This is 18 sequential fresh children: three trials of both modes at three
pacing intervals. The initial synchronous graph, pinned 3,000 x 100D GloVe
inputs, 100 held-out queries and 16 replacements / 4 deletes before every query
are unchanged. At ef=100, maintenance calls the explicit tick before each batch,
starting only when the active graph is absent or reaches 128 IDs / 4 MiB.
It does not eagerly rebuild a healthy graph. This differs from the original
`background` policy, which starts immediately after every terminal/idle poll.
The native flag is `--rebuild-mode maintenance`; budgets and high-water marks
use Rust defaults rather than additional benchmark tuning parameters.

`background.policy.capacity_samples` records post-write/pre-query graph
readiness, pressure, active used/remaining ID and vector-byte capacity, worker
snapshot/processed records, completion/terminal reason and catch-up capacity.
The snapshot is diagnostic, not a prediction of writes remaining or time to
completion. Sampling/formatting is included in full-request latency. Every
attempt, rejection, publication and catch-up overflow is retained; overload
must not be presented as a low-latency successful query. All current results
remain VLite-only Windows measurements, not portable performance claims.
Pacing creates different write pressure, **not a controlled external write
arrival rate**: oracle time, CPU contention and request work also affect it.

## Publishing results

### Exact metrics and cosine ANN

VLite 0.4.1 `vlite-bench` accepts `--metric squared_l2|cosine|inner_product`
before `--filter-every`/`--ann`. Omission preserves L2. Cosine ANN uses the same
explicit metric for construction, traversal, buffered updates and its independent
exact oracle. Inner-product ANN is rejected. Zero-norm dataset records are
excluded for cosine; zero queries fail explicitly. Empty eligible sets report
exact recall 1 by convention (there are no expected neighbors).

```powershell
cargo build --locked --release --bin vlite-bench
python benchmarks\metric_campaign.py --binary target\release\vlite-bench.exe --output metric-results.json
```

The bounded campaign runs three independent trials on 1,500 seeded records at
128 and 384 dimensions, 30 queries, k=10. It compares all three exact metrics,
10%-eligible metadata filters, and cosine ANN at ef=32/100 plus 16 mixed writes
per query at ef=100. Trial order reverses to reduce fixed-order bias; each child
has independent OS peak-RSS measurement. These are synthetic fixed-point
vectors, **not trained embeddings**. External inputs remain available through
`--vectors`/`--query-vectors` on the child CLI; record data/model provenance
before interpreting results on real embeddings.

Reports preserve every argument/raw sample, source and binary hashes, dirty
state and host details. Medians summarize per-trial percentiles, not pooled
samples. Memory covers the whole benchmark process, not incremental index RAM.
No host isolation, broad recall guarantee or production tail-latency estimate
is implied. Mixed-query times include any synchronous overflow rebuild.
See [results](RESULTS.md) before choosing an index; exact can win on small data.

Commit raw complete JSON under `benchmarks/results/` and summarize in
`RESULTS.md`: exact command, source/image provenance, machine/resources,
sample counts, repeats, p50/p95/p99, recall, indexing cost and caveats.
Use a source commit plus any dirty-state disclosure; the results commit itself
will naturally differ from the tested source. Never publish tokens, Docker
environment dumps, private datasets or internal host addresses. Label toy runs
as smoke measurements. Do not claim a winner from one workload.

## Concurrent readers

Three-run REST comparison of committed `HEAD` versus an isolated snapshot of the
reader changes ([raw JSON](results/concurrent-readers-windows.json)). Both were
built as release `vlite-server` v0.3.0 binaries from commit
`34c491b61189b6c7b397466bd5224dffde7c8a3a`; the after snapshot was dirty and
included the reader-lock and parallel exact-search work. It was captured before
later unrelated observability/MCP edits landed in the live checkout. The two
binary SHA-256 hashes, source state, full trial metrics, and database format are
in the JSON.

Measured natively on Windows 11 because WSL had no `cargo`: Intel Xeon Platinum
8370C, 8 physical / 16 logical cores. A per-trial host CPU gate waited for total
CPU load at or below 50%; observed pre-trial loads were below that threshold.
Each run used 20,000 x 128 clustered synthetic vectors, k=10, 2 s warmup and a
10 s measurement window, with one persistent HTTP/1.1 connection per worker.
Exact uses `mode=exact`; HNSW was built outside timing and queried with
`mode=approximate_ready`, `ef_search=100`. Table entries are median QPS and
median per-trial p50/p95/p99 latency; errors are baseline/after.

| Search | Readers | Baseline QPS; p50/p95/p99 ms | After QPS; p50/p95/p99 ms | QPS change | Errors |
|---|---:|---:|---:|---:|---:|
| Exact | 1 | 483; 2.00 / 2.51 / 3.21 | 788; 1.25 / 1.41 / 1.65 | +63% | 0 / 0 |
| Exact | 4 | 636; 6.11 / 7.21 / 8.52 | 1,529; 2.62 / 3.22 / 3.52 | +140% | 0 / 0 |
| Exact | 8 | 634; 12.27 / 14.16 / 18.30 | 1,927; 4.22 / 5.95 / 6.89 | +204% | 0 / 0 |
| Exact | 16 | 629; 24.77 / 29.44 / 35.89 | 2,155; 7.58 / 11.37 / 12.77 | +243% | 0 / 0 |
| HNSW | 1 | 1,528; 0.64 / 0.78 / 0.97 | 1,560; 0.62 / 0.76 / 0.97 | +2% | 0 / 0 |
| HNSW | 4 | 3,521; 1.13 / 1.30 / 1.67 | 4,038; 0.95 / 1.37 / 1.65 | +15% | 0 / 0 |
| HNSW | 8 | 3,491; 2.26 / 2.54 / 3.75 | 4,310; 1.77 / 2.92 / 3.59 | +23% | 0 / 0 |
| HNSW | 16 | 3,447; 4.58 / 5.16 / 8.38 | 4,648; 3.22 / 6.18 / 7.80 | +35% | 0 / 0 |

Single-reader exact on the separate 100,000 x 768 dataset (three 10 s runs):

| Version | QPS | p50 / p95 / p99 ms | Errors |
|---|---:|---:|---:|
| Baseline | 22.2 | 44.76 / 46.60 / 48.70 | 0 |
| After | 98.2 | 10.24 / 11.13 / 12.16 | 0 |

The exact concurrent path gains substantially as shared readers avoid the old
serialization; throughput still flattens at high reader counts under this
8-physical-core host. The 100k exact result is about 4.4x faster after, combining
the new Rayon scan with the server read-lock change. HNSW's single-reader result
is nearly unchanged, with a more modest 35% QPS gain at 16 readers. These are
synthetic, warm-cache, single-host REST measurements with no concurrent writes;
they are not production tail-latency guarantees. The optional embedded
multi-process benchmark was not run.
