# Benchmarking VLite

Use benchmarks to answer a workload question, not to produce a universal ranking.
Publish configuration, correctness/recall, raw measurements and limitations with
every result. [Published runs](RESULTS.md) follow this policy.

## Native VLite benchmark

### HNSW visited-membership optimization (unreleased source)

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
small warm-cache before/after experiment on a shared host, not evidence
of a universal gain. No results were collected at
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

This harness and its new CLI flags are current-source additions after v0.1.2.
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
for failure, overload, shutdown and current-source compatibility details.

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

Current-source `vlite-bench` accepts `--metric squared_l2|cosine|inner_product`
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
