# Published performance measurements

Results are workload observations, not universal product rankings.
See [methodology and reproduction](README.md) before interpreting the numbers.

Cross-database measurements and raw run provenance are added only after
successful execution and ground-truth validation. No zvec, Milvus, Weaviate,
FAISS or other unmeasured engine results are inferred.

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
