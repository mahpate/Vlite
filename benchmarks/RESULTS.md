# Published performance measurements

Results are workload observations, not universal product rankings.
See [methodology and reproduction](README.md) before interpreting the numbers.

No zvec, Milvus, Weaviate, FAISS or other unmeasured engine results are inferred.

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
