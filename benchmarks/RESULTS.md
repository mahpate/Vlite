# Published performance measurements

Results are workload observations, not universal product rankings.
See [methodology and reproduction](README.md) before interpreting the numbers.

No zvec, Milvus, Weaviate, FAISS or other unmeasured engine results are inferred.

## Background rebuilds: latency versus availability, October 20, 2026 UTC

[Paired raw report](results/glove-100-background-windows.json) measures the new
opt-in Rust lifecycle against synchronous lazy rebuilding on the **same
current-source binary**. It reuses the licensed 3,000-row GloVe 100D subset,
100 queries, 16 real replacements plus four deletes before every query,
cosine k=10/ef=100/m=16/ef-construction=100. Three trials per mode/pacing setting,
12 fresh children; order reverses on alternate trials. Both modes build the
initial graph synchronously outside measured requests.

Availability is the minimum fraction of attempted queries served in any trial.
Recall in the table is over **served queries only**; rejected requests have
zero delivered recall and must not be counted as successful fast queries.
Latency values are medians of trial percentiles, not pooled percentiles.
Request timing includes owner-thread polling/publication, snapshot cloning,
mutation generation, durable commit/live-state update and the query. Pacing,
exact-oracle validation and final cancellation/close are excluded.

| Mode / pre-request sleep | Availability | Served recall@10 | Served query p99 (ms) | Request p99 (ms) | Whole-child peak RSS (bytes) |
|---|---:|---:|---:|---:|---:|
| Synchronous / none | 100% | 1.000 | 1961.8581 | 1963.4961 | 11,546,624 |
| Background ready-only / none | 12% | 1.000 | 1.3406 | 2.3714 | 13,201,408 |
| Synchronous / 250 ms | 100% | 1.000 | 1920.5720 | 1922.0922 | 11,542,528 |
| Background ready-only / 250 ms | 57% | 1.000 | 2.2519 | 4.6165 | 14,209,024 |

Without pacing, the worker could not catch up: all three trials served 12/100
requests, published no replacement, and reported seven catch-up overflows.
With 250-ms sleeps, trials served **57, 58, 57** requests and each published
12 replacement graphs without catch-up overflow. Buffered mutations aged
the published graphs: their remaining capacity was consumed before the next
replacement completed, so 42--43 ready-only requests still returned
`IndexNotReady`. Aggregate delivered recall (counting rejects as zero) was
0.12 unpaced / at least 0.57 paced, **not 1.0**.

All background trials performed **zero synchronous query-triggered rebuilds**.
The largest background request was 5.2689 ms unpaced / 10.6932 ms paced;
maximum observed owner maintenance (poll/start) was 1.6333 ms.
Synchronous mode served every request but rebuilt seven times per trial,
with maximum request latency 2021.2781 ms unpaced / 1991.1180 ms paced.
These are observed maxima on this small shared-host workload, not upper bounds.
Background construction consumes CPU and additional RAM; the snapshot and two
graphs can coexist. RSS is a whole-child high-water mark, not marginal index RAM.

**Conclusion:** the new API removes graph construction from ready-only requests,
with explicit cancellation, bounded catch-up and no stale publication, but
**does not solve continuous ANN availability at this mutation rate**.
Choose it only when fast explicit rejection/retry or application-controlled
maintenance is acceptable. Use exact search when recall/availability matter
more than approximate-index throughput; retain synchronous mode if waiting is
preferred to rejecting. Do not market this as an unconditional latency or
availability improvement, or increase buffers silently to hide overload.
Sleeping before requests is not a fixed arrival-rate load test, and oracle
work also gives workers time to progress.

The lifecycle is current-source **Rust-only**, after v0.1.2. REST and C/Python/Node
interfaces remain synchronous/ready-only as before. No cross-platform performance,
production SLA or crash-durability claim follows from this Windows x64 campaign.
See [reproduction and timing definitions](README.md#background-rebuild-latency-and-availability)
and [lifecycle limits/errors](../docs/REFERENCE.md#background-hnsw-rebuilds-rust).

## Real GloVe cosine retrieval and mixed deletes, October 20, 2026 UTC

[Raw campaign and dataset provenance](results/glove-100-windows.json) records
24 native Windows x64 release children: three trials on 3,000 real 100D
GloVe Twitter word vectors, 100 held-out upstream test rows, k=10.
[Preparation, license, exact subset ranges and reproduction](README.md#real-glove-embeddings-and-mixed-deletions)
are documented; vectors are not included. Every exact result and post-mutation
ANN candidate score is checked against an independent current-state oracle.
The upstream full-dataset neighbor labels are not used.

Values are medians of trial percentiles; recall is the minimum trial mean.
Identical inputs/graph seed mean the trials repeat timing, not independent
randomized recall experiments. Static and mixed modes are different workloads.

| Mode | p50 (ms) | p95 (ms) | p99 (ms) | Minimum trial recall@10 |
|---|---:|---:|---:|---:|
| Exact cosine | 0.7419 | 1.2637 | 1.7236 | 1.000 |
| Filtered exact cosine, 10% eligible | 0.1358 | 0.2331 | 0.5277 | 1.000 |
| Static cosine ANN, ef=32 | 0.1814 | 0.2966 | 0.3412 | 0.916 |
| Static cosine ANN, ef=100 | 0.4625 | 0.5835 | 0.6531 | 0.995 |
| Static cosine ANN, ef=300 | 0.8000 | 1.0541 | 1.2217 | 1.000 |
| Mixed cosine ANN, ef=32 | 0.8808 | 2052.9031 | 2215.3210 | 0.992 |
| Mixed cosine ANN, ef=100 | 0.9480 | 1922.6594 | 2241.4217 | 1.000 |
| Mixed cosine ANN, ef=300 | 1.1723 | 1878.5783 | 2089.2876 | 1.000 |

Each mixed query follows 16 real replacements and four deletes in one durable
batch. Every trial rebuilds on queries **13, 26, 39, 52, 65, 78, 91** after the
256-distinct-ID buffer limit is crossed. The write and triggering query are
timed separately; query latency includes the complete synchronous rebuild.
At ef=100, non-rebuild query median was 0.9432 ms, but the maximum observed
query was **2776.3046 ms**. Do not hide that pause behind a steady-state number.
Each child finishes with 2,600 live records and 677,200 additional logical
database bytes; deletion does not reclaim the append log.

Static ef=100 initial index build median was 2366.3051 ms. Whole-child median
peak RSS was 10,309,632 bytes for exact, 10,133,504 bytes for static ef=100,
and 11,370,496 bytes for mixed ef=100. This includes all input vectors,
database, full-sort oracle, index and mutation history; differences are
**not measurements of marginal index memory**. The mixed report's initial
exact timing is pre-mutation; no timed mixed exact baseline was collected.

**Recommendations for this measured subset:** use exact search for strict
recall, selective filters, or when avoiding HNSW rebuild pauses matters.
Static ef=100 is the lowest tested setting meeting the declared 99% mean-recall
target in every trial; it is a candidate for read-heavy workloads, not a new
default or universal guarantee. Its worst individual query recall was 0.9;
ef=32 fell to 0.3 on a static query despite its faster median.
Ef=300 reached full measured static recall but was slower than exact at p50.
The mixed ef=32 mean exceeding 99% does not establish equivalent graph quality:
buffered vectors are scanned exactly and the live dataset changes.
Current buffered ANN is **not a bounded-tail recommendation for sustained
distinct-ID mutations**; schedule explicit rebuilds outside latency-sensitive
traffic, use ready-only queries to reject missing indexes, or stay with exact
search. Ready-only mode does not eliminate build cost or make writes concurrent.

This is a small prefix subset of upstream shuffled splits, not the full
million-vector dataset, modern sentence embeddings, or a production workload.
The Windows host was shared/unpinned; 100 queries and three timing repetitions
do not establish a production tail SLA. No Linux/macOS performance claim or
crash-durability claim follows. The report identifies its precommit dirty-source
hashes and binary SHA-256, not a released v0.1.2 benchmark binary.

## Exact metrics and cosine ANN, October 20, 2026 UTC

[Final cached-norm campaign](results/metric-cosine-cached-windows.json) contains
54 native Windows x64 release children: three trials, 1,500 seeded synthetic
records, 128/384 dimensions, 30 queries and k=10. Each child checks exact results
and current ANN candidate scores against an independent full-sort oracle.
The 10%-eligible filter uses strict integer metadata. Values below are medians
of trial p50s; recall is the lowest trial mean, not a universal guarantee.

| Metric / mode | 128D p50 (ms) | 384D p50 (ms) |
|---|---:|---:|
| Exact squared L2 | 0.2350 | 0.7023 |
| Exact cosine | 0.4366 | 1.4585 |
| Exact inner product | 0.2349 | 0.7218 |
| Filtered squared L2 (10% eligible) | 0.0532 | 0.1151 |
| Filtered cosine (10% eligible) | 0.0786 | 0.2168 |
| Filtered inner product (10% eligible) | 0.0788 | 0.1074 |
| Cosine ANN ef=32 | 0.1790 | 0.4340 |
| Cosine ANN ef=100 | 0.3615 | 0.7901 |
| Cosine ANN ef=100, 16 replacements/query | 0.6899 | 1.3542 |

Static cosine recall at ef=32 was 0.830000/0.716667 (128D/384D);
ef=100 reached 0.993333/0.963333. Mixed ef=100 reached 1.000000/0.993333.
Do not choose ef=32 merely because it is faster, or infer that mixed writes
improve the graph: buffered records are scanned exactly. Mixed timings include
a synchronous rebuild when query 17 exceeds the 256-ID buffer.
Static ef=100 build time was 989.5571/2520.4525 ms; query p95 was
0.5682/0.8993 ms. Whole-child median peak RSS was 7,159,808/10,338,304 bytes,
including input vectors, database, oracle, graph and all phases; this is
**not incremental index memory**. Exact cosine whole-child RSS was
6,529,024/9,801,728 bytes. Filtered cases additionally store metadata and should
not be interpreted as query-memory comparisons.

The [initial development campaign](results/metric-cosine-windows.json) computed
norms repeatedly during traversal. Caching original graph-node norms and
computing each query norm once reduced static ef=100 observed p50 from
0.7136/1.8957 ms to 0.3615/0.7901 ms. The recorded dataset/query checksums and
per-trial recall match before/after caching. These were **sequential exploratory
campaigns**, not alternating old/new binaries under controlled load. Exact
cosine latency also drifted upward, and some filtered runs were slower.
No broad speedup or unchanged-resource claim follows. The uncached development
source was not a release; both reports identify dirty source hashes and binary
SHA-256 rather than pretending to measure their base commit's released code.

Reproduce the final source with the command in
[the metric campaign guide](README.md#exact-metrics-and-cosine-ann).
These fixed-point vectors are not trained embeddings, the host was shared and
not pinned, and 30 queries are insufficient for a stable production tail.
No real-embedding, non-Windows performance or general recall result is claimed.
Exact search can still be preferable for small datasets, selective filters or
strict recall requirements. Cosine ANN is available in v0.1.2+;
the v0.1.1 packages do not contain it.

## Exact-search distance cutoff, October 19, 2026 UTC

[Raw per-query samples and binary/source hashes](results/exact-cutoff-windows.json)
compare baseline commit `6645183` with the cutoff implementation. Both are native
Windows x86_64 release binaries on the same shared host. Each workload has
3 alternating-order trials, 3,000 records, 100 queries, k=10 and seed 42.
The existing native benchmark checks every query against its full-sort oracle;
all runs returned recall 1.0 and matching input checksums.

| Synthetic workload | Baseline median trial p50 (ms) | Cutoff median trial p50 (ms) |
|---|---:|---:|
| Uniform, 8 dimensions | 0.0452 | 0.0473 |
| Uniform, 128 dimensions | 0.4847 | 0.4746 |
| Uniform, 768 dimensions | 3.2106 | 3.0716 |
| Ordered separable, 128 dimensions | 0.4814 | 0.1584 |

The separable fixture intentionally favors pruning: its first 20 records have
every component `(id % 10) * 0.01`, the remaining records have every component
`10 + (id % 10)`, and all queries are zero vectors. Earlier IDs establish a
small cutoff before distant records are scanned. This is not a representative
embedding dataset. Uniform cases use the benchmark's existing SplitMix64
generator. The small-dimensional result was slower in this run; do not claim
a universal improvement or extrapolate these timings to REST, other CPUs or
real embeddings. No CPU pinning or resource isolation was imposed.

Reproduce uniform cases with each release binary using
`vlite-bench 3000 <dimensions> 100 100 10 42`. For the separable case, write one
whitespace-separated vector per line using the recipe above and run
`vlite-bench --vectors <vectors-file> --query-vectors <queries-file> 3000 128 100 100 10 42`.
Keep the baseline and candidate binaries separate and alternate trial order.
The unchanged short-vector path remains a full distance calculation; larger
vectors check a nonnegative partial sum after each 32 components. Returned
distances still accumulate all components in the original f64 order.

## Python embedded binding and batching, October 19, 2026 UTC

[Raw timings and binary/source hashes](results/embedded-python-windows.json).
This is a **binding-level microbenchmark**, not REST or Rust-only engine
throughput: Windows 11 x86_64, Python 3.15.0, release DLL, shared host with
16 logical CPUs, 128 synthetic vectors x 32 dimensions, seed 42, three
fresh-database trials, batches of 32 and ten warm read passes per trial.

| Operation | Median records/second |
|---|---:|
| Individual durable puts | 286.32 |
| Atomic batch puts (32 records/commit) | 7,560.28 |
| Individual cached gets | 41,618.57 |
| Bounded `get_many` (32 IDs/call) | 66,455.53 |

Each timed write trial covers all 128 records; each timed read pass reads all
128. Throughput is 128 divided by the median elapsed duration, not a sustained
service rate. Write figures have three samples each; read figures have 30.
Operation order alternates; input generation, correctness comparisons,
creation/open/close and read warmups are excluded. Raw samples include host
variation; no CPU pinning or resource isolation was imposed.

**Interpretation:** batch writes amortize sync over 32 operations, changing the
acknowledgment/atomicity granularity; this is not faster per-record durability.
`get_many` reduces foreign-function calls and returns ordered records/nulls
with strict count/response bounds. These small cached-read results do not
establish benefits for large payloads, concurrent clients or cold storage.
All returned contents and exact search were checked independently. No claim is
made about search throughput, tail latency, physical power-loss durability or
other databases.

Reproduce using the matching platform library:

```text
cargo build --locked --release -p vlite-ffi
python benchmarks/embedded.py --library target/release/vlite_ffi.dll --output embedded-results.json
```

## Native 384-dimensional mixed-update profiling, October 18, 2026 UTC

This separate **VLite-only synthetic diagnostic** uses direct embedded calls,
not REST. It must not be ranked against the HTTP measurements below.
[Raw campaign and all child samples](results/native-384-mixed.json).

Windows 11 x86_64, Intel Xeon Platinum 8370C at 2.80 GHz, release binary,
1,024 vectors x 384 dimensions, 30 queries, k=10, seed 42, m=16, construction
ef=100, three fresh-database trials per configuration. Shared host, no CPU
pinning or imposed process resource limits. Higher dimension is not proof of
real-embedding representativeness: these remain seeded uniform vectors.

All latencies are milliseconds. Values are medians of trial-level percentiles;
recall is the minimum of the three trial means. The target is 0.99 in **every**
trial. Mixed rows commit 16 replacement vectors before each ANN query and
include the buffer-overflow rebuild in ANN timing.

| Updates/query | Search ef | ANN p50 | ANN p95 | ANN p99 | Minimum recall@10 | Target met |
|---|---|---|---|---|---|---|
| 0 | 32 | 0.3949 | 0.4791 | 0.5286 | 0.846667 | No |
| 0 | 100 | 0.6403 | 0.7173 | 0.8515 | 0.996667 | Yes |
| 16 | 32 | 0.9103 | 1.1620 | 1753.4897 | 0.993333 | Yes |
| 16 | 100 | 0.9392 | 1.0910 | 1721.8457 | 1.000000 | Yes |

**Finding:** low-ef static ANN looks faster but misses the recall target.
Static exact-scan p50 was approximately 0.51 ms, so static ANN at ef=100 did
not beat exact scanning at this size. Mixed-workload recall is higher partly
because buffered vectors are scanned exactly; this is not evidence that
updates improve the graph. Exact timings in each child precede mutations and
are not timed post-write baselines.

Every mixed trial's slowest query was query 17: the 17th batch crosses the
256-distinct-ID buffer limit and triggers a synchronous full rebuild. Initial
build medians were 1.71-1.75 seconds, consistent with the observed pause.
There are only 30 queries per trial, so nearest-rank p99 is the maximum sample,
not a statistically stable production p99. Do not hide this pause behind p50,
or claim the mutation buffer eliminates rebuild costs.

Median OS-measured whole-child peak RSS was 8,863,744-8,912,896 bytes for static
runs and 9,621,504-9,662,464 bytes for mixed runs. These are total high-water
marks including input generation, database, oracle and graph, **not marginal
index/buffer RAM**. The logical database file grew from 1,589,852 to 2,336,372
bytes after 480 replacements in 30 durable batches: 746,520 bytes of log growth.
Static runs had no log growth. Filesystem allocation/compaction were not measured.
Each write batch and query sample is retained separately in the raw report.

Reproduction from this checkout:

```powershell
cargo build --locked --release --bin vlite-bench
python benchmarks\native_campaign.py --binary target\release\vlite-bench.exe --records 1024 --dimensions 384 --queries 30 --trials 3 --ef 32 100 --mixed-writes 16 --recall-target 0.99 --output native-384.json
```

The run's base commit is `61180befc1903491d6b3e0fe8b410bf6d2125f93`
with `source_dirty=true` for the new benchmark implementation. Exact benchmark/
engine/runner source-byte hashes and the binary SHA-256 are in the report.
Binary SHA-256:
`20f5babe764f9b2368aaf49125ce678aafe5298a75953983a9e17b12158e63f3`.
The report is copied without modifying measured samples.

This establishes a rebuild-tail bottleneck, not a CPU-stack attribution to
distance computation or allocations. No real-embedding, concurrent, Linux
native, zvec, or larger-scale performance result is implied. The
[benchmark guide](README.md#native-profiling-real-vectors-and-mixed-updates)
documents external input support and measurement boundaries.

## VLite vs Qdrant: authenticated REST, October 18, 2026 UTC

**Scope:** synthetic development-host comparison, not a production capacity
study or a leaderboard. VLite is not uniformly faster. Qdrant was faster on
unfiltered exact and ANN queries; VLite was faster on this unindexed selective
filter workload. The dataset is small, and HTTP/Python overhead is significant.

Three fresh-data trials per engine/configuration, 10,000 vectors, 32 dimensions,
100 measured queries/trial, 10 warmups, k=10, seed 42, batch size 100,
concurrency 1, persistent HTTP connections. Each engine ran separately with
2 CPU / 2 GiB Docker limits and local named-volume storage. Engine order
alternated by trial. Host: Intel Xeon Platinum 8370C at 2.80GHz, WSL2
Ubuntu 24.04.5, kernel 6.6.87.2, Docker 29.8.1, x86_64, 16 logical CPUs and
approximately 31.3 GiB available to Docker. This is not a dedicated isolated
bare-metal machine. TLS is not included; API authentication is included.

The table reports the **median of the three per-trial percentiles**, not
percentiles pooled across all 300 queries. Recall is the per-trial mean,
reported as a range when it varies. Latencies are milliseconds.

| Workload | Engine | p50 | p95 | p99 | Tie-aware recall@10 |
|---|---|---|---|---|---|
| Exact, unfiltered | VLite | 2.2025 | 2.4673 | 2.7841 | 1.000 |
| Exact, unfiltered | Qdrant 1.14.1 | 1.8609 | 2.4376 | 2.8580 | 1.000 |
| Exact, 10% eligible | VLite | 1.8397 | 2.3313 | 2.6636 | 1.000 |
| Exact, 10% eligible | Qdrant 1.14.1 | 2.1600 | 2.8217 | 3.0282 | 1.000 |
| HNSW, unfiltered | VLite | 2.5324 | 2.8284 | 2.9032 | 0.988 |
| HNSW, unfiltered | Qdrant 1.14.1 | 1.7545 | 2.1202 | 2.2258 | 0.992-0.995 |

Exact filtering uses a boolean match selecting 1,000 records; no scalar index
was created for either engine. HNSW uses m=16, construction ef=100, search ef=100,
and no quantization. Qdrant indexed all 10,000 vectors before timing; its
`full_scan_threshold` was set to its supported minimum 10 KiB for the HNSW track.
VLite builds on the initial ANN query. These settings do not equalize recall:
the ANN rows are **not a matched-recall comparison**. At this small dimension/
dataset size VLite's graph was slower than its exact scan.

Median separately measured index-wait-plus-first-query time was approximately
7.96 s for VLite and 1.62 s for Qdrant. The latter may overlap indexing with
ingestion, so this is **not comparable isolated index-build CPU time**.
Ingestion timings are present in raw JSON but deliberately not ranked:
VLite batch sync acknowledgment and Qdrant `wait=true` have different
durability contracts. Peak memory/disk use, concurrent load, mixed writes,
cold-cache behavior, physical power failures and public-network TLS were not
measured here. Tail estimates from 100 queries are noisy.

### Raw measurements and provenance

- [Exact unfiltered](results/rest-exact-10k.json)
- [Exact filtered](results/rest-filtered-10k.json)
- [Explicit indexed ANN](results/rest-ann-indexed-10k.json)
- [Supplementary default-planner ANN request](results/rest-ann-10k.json)

The supplementary run retained Qdrant's default 10,000 KiB scan threshold.
Although all vectors were indexed, the small workload could use the scan
planner; Qdrant's recall was 1.0 in all trials. It is preserved for transparency,
not labeled an HNSW-vs-HNSW measurement.

Dataset-plus-queries SHA-256:
`4cd1a47cde5ede3003fc4d34ba95695f9ba972916af8bb0995d9ab188411e457`.
VLite source/image build revision:
`0a59a7e0ab95059a45d7a9271d33a5747f9e6f5e`.
The explicit ANN runner revision is
`0c5c5c1bb27d3bd4f91093f735e4e91ebfec1fa3`; intervening changes only corrected
the Qdrant planner configuration and benchmark documentation.
All reports record source-tree hashes, image IDs, full query samples, server
configuration, index counts, and run-to-run variation.

VLite local image identity:
`sha256:ba7e891b3a1bdfe46d3920fd00d8c01f0b4af2cf209fe16591b944d0353bd6d9`.
Qdrant image identity:
`sha256:419d72603f5346ee22ffc4606bdb7beb52fcb63077766fab678e6622ba247366`.
These are local Docker image identities, not a claim that a VLite image was
published to a registry. The measured Qdrant release is pinned, not a claim
to be the newest release or tuned optimally.

The reports' `git_dirty=true` reflects WSL Git treating the Windows checkout's
CRLF files as changes. Windows Git was clean at each source checkpoint;
`git diff --ignore-space-at-eol --exit-code` confirmed no content differences
before the corrected ANN campaign. Byte-level source hashes are included.
For publication only the personal output-directory path was reduced to its
basename; measured samples were not changed.

Reproduce with the commands in [the benchmark guide](README.md), setting
`--vlite-image` to the matching locally built source. Match image versions,
resources and workload—not only the command defaults. These results justify
further testing, not choosing a database without application-level evaluation.

## Prior native exact-selection comparison

Windows x86_64 release binaries, 32 dimensions, 100 seeded queries, seed42,
one sequential run per configuration. Timings include Rust query execution
and result allocation, not HTTP. The old binary used full-sort selection; the
new binary used a bounded top-k heap.

| Records | k | Full-sort p50 (ms) | Bounded heap p50 (ms) | Exact recall |
|---|---|---|---|---|
| 1,000 | 10 | 0.1401 | 0.0612 | 1.0 |
| 1,000 | 1,000 | 0.1143 | 0.1339 | 1.0 |
| 10,000 | 10 | 1.8302 | 0.7373 | 1.0 |
| 10,000 | 1,000 | 1.7768 | 0.9661 | 1.0 |

These earlier development observations have no archived per-query samples and
are not the controlled cross-database campaign. Large k can make heap selection
slower; there is no unconditional performance-improvement claim.
