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

```text
cargo run --locked --release --bin vlite-bench -- 10000 32 100 100 10 42
cargo run --locked --release --bin vlite-bench -- --ann 16 100 100 10000 32 100 100 10 42
cargo run --locked --release --bin vlite-bench -- --filter-every 10 10000 32 100 100 10 42
```

Measure whole-child peak memory separately using
`tests/measure_benchmark.py`. That peak includes dataset generation and the
full-sort oracle, not only query working memory. Never present input-vector
byte counts as measured process memory.

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

## Adding zvec or another engine

For zvec, use its [collection schema and optimization API](https://zvec.org/en/docs/db/quickstart/)
in a separate in-process adapter. Pin the package/wheel and index parameters;
create the same vectors/IDs/scalar field; use L2 rather than default cosine;
time ingestion, `optimize()`, warmups, queries and mixed mutations separately.
Check scores, ties and recall against the same oracle before trusting timings.
Report Python-to-native overhead versus Rust direct-call overhead, or expose
both through equivalent wrappers and measure that track separately.

FAISS is an index library, not a durable vector database; durability/CRUD
comparisons require an application storage layer. For Milvus/Weaviate, record
all supporting services and total resource allocation, not just the query node.
Do not borrow marketing numbers from upstream benchmark pages for our table.
No external engine numbers are published here until its adapter has actually run.

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
