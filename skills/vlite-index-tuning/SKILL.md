---
name: vlite-index-tuning
description: Tune VLite exact and HNSW retrieval with metadata indexes, filtered ANN, scalar-u8 quantization and recall measurements. Use when asked to improve search latency, choose exact versus ANN, tune ef_search or HNSW settings, measure recall, or reduce index memory.
---

# Tune VLite retrieval

## Measure a baseline

Record collection size/dimensions, model, metric, filter selectivity, concurrency,
CPU/RAM, k and latency target. Use representative held-out production queries,
not just stored vectors. Keep exact top-k as ground truth under the **same metric
and filters**. Compare recall@k, p50/p95 latency, throughput, build/rebuild time
and RSS, not one fastest query.

## Select the search path

- **Exact**: small collections, strict correctness, inner product or selective
  filters. Supports `squared_l2`, `cosine`, `inner_product`.
- **Metadata indexes + exact**: declare `eq`, `range` or `eq_range` for recurring
  predicates. Explain stats show whether candidates actually shrink. Conditions
  are ANDed typed predicates, not arbitrary JSON.
- **HNSW**: larger collections where measured speed/recall beat exact. Only
  squared L2/cosine. Build explicitly and check configuration/readiness before
  ready-only queries; avoid first-request lazy builds.
- **Auto**: selects scan/indexed-exact or ready ANN; never builds. Inspect the
  plan rather than claiming it always chooses ANN.
- **Filtered ANN**: check deployed support; REST advertises
  `features.filtered_ann`. Keep predicates inside search. Post-filtering an
  unfiltered top-k loses eligible neighbors and is not authorization. Fewer than
  k may return; ef is not a hard graph-visit/time cap.

See [configuration and experiments](references/tuning.md) for verified API/CLI
examples. Embedded Python `search_approximate_ready` has no filter argument;
use exact/filtered `search_auto` or the REST filtered-ANN surface instead.

## Tune incrementally

1. Align the metric with the embedding model; cosine queries cannot be zero.
   Change one variable per experiment.
2. Begin unquantized. Raise `ef_search` to trade query work for recall. It must
   be at least `min(k, live_records)` and within engine bounds.
3. Sweep `m`/`ef_construction` if query tuning is insufficient. Larger connectivity/
   construction breadth generally cost build time/memory; measure rather than
   assuming strictly monotonic recall.
4. Test `quantization="scalar_u8"` separately. Increase `rerank_multiplier` to
   rescore more candidates with original vectors. Rescoring cannot guarantee
   global recall and is not language-model/cross-encoder reranking.
5. Test rare/common filter values and after writes/deletes. Inspect buffer/stale
   graph state and settings identity. Rebuild as needed, budgeting CPU/RAM and
   operational disruption.
6. Re-run held-out exact comparisons. Report workload/recall/limitations; persist
   selected settings only deliberately.

## Memory and exact-search concurrency

Scalar-u8 reduces traversal code storage/work, but original f32 records remain
resident. Component memory is **not RSS**, peak build RAM or a fourfold total
memory-savings promise. Snapshots cache derived state, not records/database replay.

Large native exact scans automatically use the shared Rayon pool when row/work
thresholds are met (currently at least 4,096 candidate rows and 524,288
row×dimension work with multiple workers). No Python `parallel=True` keyword
exists. Set `RAYON_NUM_THREADS` before process/pool initialization if needed;
measure CPU oversubscription. Small/selective scans stay serial. WASM lacks this
parallel path. Merging preserves exact distance/ID ordering.

Distinguish intra-query parallelism from parallel requests: C ABI calls serialize
in one process. Multiple Python processes need separate shared read-only opens,
which exclude writers. Use the server for coordinated mutable access.

## Deliver a reproducible result

Report workload/seed/model, baseline, chosen settings, recall/latency, memory
measurement method and build cost. Never silently change production or present
synthetic tune recall as universal quality. Use `vlite-ops` for backup/ownership.
