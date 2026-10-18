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

## Publishing results

Commit raw complete JSON under `benchmarks/results/` and summarize in
`RESULTS.md`: exact command, source/image provenance, machine/resources,
sample counts, repeats, p50/p95/p99, recall, indexing cost and caveats.
Use a source commit plus any dirty-state disclosure; the results commit itself
will naturally differ from the tested source. Never publish tokens, Docker
environment dumps, private datasets or internal host addresses. Label toy runs
as smoke measurements. Do not claim a winner from one workload.
