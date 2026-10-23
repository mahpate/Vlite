# Configuration and reproducible tuning

## Embedded Python settings and explain

Create once within an open format-4+ database:

```python
db.create_collection("docs", 384, settings={
    "metric": "cosine",
    "hnsw": {"m": 16, "ef_construction": 128, "seed": "42"},
    "quantization": "none",
    "rerank_multiplier": 4,
    "metadata_indexes": [
        {"field": "tenant", "kind": "eq"},
        {"field": "created_at", "kind": "range"},
    ],
})
```

Use real 384-dimensional embeddings, not the three-dimensional RAG toy data.
Embedded `db.set_collection_settings(name, settings)` is a full replacement;
keep all intended fields. REST `client.set_collection_settings` retains omitted
fields. Inspect `persisted` on legacy files.

```python
scope = [{"op": "eq", "field": "tenant",
          "value": {"type": "string", "value": "demo"}}]
# query is a real nonzero 384-dimensional embedding from the stored model.
baseline = db.search(query, 10, collection="docs", metric="cosine",
                     filter=scope, explain=True)
print(baseline["stats"])
db.build_index(collection="docs", metric="cosine", quantization="scalar_u8",
               rerank_multiplier=4)
planned = db.search_auto(query, 10, collection="docs", metric="cosine",
                         filter=scope, ef_search=100, quantization="scalar_u8",
                         rerank_multiplier=4, explain=True)
print(planned["plan"], planned["stats"])
```

Auto may choose exact; label a measurement ANN only if its plan says so. Stats
include distance evaluations, graph visits, buffered merges, reranked candidates
and index candidates. `db.ann_memory(collection="docs", metric="cosine",
quantization="scalar_u8", rerank_multiplier=4)` reports components.
`db.ann_quality(collection="docs", metric="cosine", quantization="scalar_u8",
rerank_multiplier=4, samples=8, k=10, ef_search=100)` performs budgeted seeded
comparison for the matching ready graph. Check skipped reasons/samples used.

REST uses `docs = client.collection("docs")`, `docs.build_index(...)`,
`docs.index_status()` and `docs.search_detailed(request)`. Filtered ANN request:
`{"vector": query, "k": 10, "metric": "cosine", "mode": "approximate_ready",
"filter": scope, "ef_search": 100, "quantization": "scalar_u8",
"rerank_multiplier": 4, "explain": True}`. Require advertised filtered ANN.
Readiness/config mismatch is an error, not a silent exact fallback.

## Offline tuning and quality tools

Build with Rust 1.89+ from the checkout:

```powershell
cargo build --locked --features tools --bin vlite-tune --bin vlite-quality --bin vlite-bench
```

Stop **all** source owners including readers. Tools acquire exclusive ownership.
Run installed binaries from PATH or their `target\debug`/`target\release` paths:

```text
vlite-tune --db example.vlite --m 8,16,32 --ef-construction 64,128 --ef-search 20,50,100 --k 10 --samples 8 --target-recall 0.95 --json
vlite-quality --db example.vlite --k 10 --samples 8 --json
vlite-quality --db example.vlite --m 8 --other-m 32 --other-ef-search 200 --json
vlite-quality --db before.vlite --other-db after.vlite --json
vlite-bench --help
```

Verified against `docs/TOOLS.md`, `src/bin/vlite-tune.rs` and `tests/tune_cli.rs`.
These are not `vlite-maintain tune` commands. `vlite-bench` is synthetic; inspect
its `--help` rather than borrowing tune flags. Tune/quality evaluate L2 rather
than an arbitrary collection metric; they do not persist settings/indexes or
modify source bytes.

Tune sweeps at most 256 combinations. Samples: 1–32; k: 1–100. Each builds a
full-collection graph, so query sampling does **not** bound build memory by sample
count. Recommendation minimizes `mean_query_us + memory_weight * component_MiB`
among candidates meeting recall (`--memory-weight`, default 1). No qualifying
candidate gives JSON null/nonzero exit; do not report that as a selected config.

Reports include build time, mean/p95 latency, recall, Jaccard overlap, rank
correlation and component-memory estimates. Stored-vector queries include self-
neighbors and can overestimate real-question recall. Small samples are not
statistically robust. Use an independent application harness for production
queries, authorization filters, concurrency and end-to-end latency.

For database comparison, IDs must mean the same thing and dimensions must match.
Each side also has its own exact baseline. Recall@k is intersection/exact count;
Jaccard is intersection/union. Rank correlation uses only shared IDs, can be null
when fewer than two are shared, and is not a missing-result penalty.

## Acceptance checklist

- Identical metric, query, filter, k and data state for exact/ANN comparison.
- Separate embedding/network and vector-search latency.
- Report warm/cold conditions, repetitions and concurrency.
- Measure RSS/peak build memory, not just component estimates.
- Test after writes/deletes/restart; inspect ready/buffer/stale state.
- Sidecars are optional plaintext caches, not canonical databases.
- Persist deliberately, verify readiness and retain a tested rollback path.
