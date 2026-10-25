# feature-agent-graph — DO-005 reference baseline (FILE-024)

Companion to [`feature-agent-graph.prd.md`](feature-agent-graph.prd.md) (DO-005, REG-010, ITEM-006) and [`decisions.md`](decisions.md). These numbers are the graph-off reference for REG-010. NFR-002 and NFR-003 must be measured on the same machine. They are measurements, not performance guarantees; the REG-010 budgets (PD-014) are still proposals for GATE-004.

## What was measured

| Item | Value |
|------|-------|
| Commit | `81ff901c54d674d1ad519dd4fa722e937db46c4b`, clean `git archive` copy (`working_tree_dirty: false`), built in its own target directory with default features, release profile |
| Profile | `reference`: the existing CI `vlite-bench` workloads plus a seeded synthetic dataset of 100,000 records × 384 dimensions |
| Runs | 5 per workload; each value below is the median over the 5 runs (not over requests inside a run) |
| Queries | 10 per run on the 100,000 × 384 workloads. A 1,000-query attempt exceeded `vlite-bench`'s 500-million distance-component limit for exact search, so the script uses 10 queries. With 10 samples, p99 equals the slowest query of a run. |
| REG-013 | `pass` (pinned artifact feature sets match `Cargo.toml`; checked with `tests\graph_build_boundary.py` against the measuring working tree) |
| Raw output | `graph-baseline-reference-81ff901.json` in the session-state directory of the measuring session (86,397 bytes, SHA-256 `0C7D5C55502560F6161AA363F24F41FC6DEDC4D61AF00BBCCD99D6F5D4F8E146`). It is not in the repository. |

**Command:** `python scripts\graph_baseline.py measure --rev 81ff901 --profile reference --runs 5 --reference-machine-confirmed --output <path>`.

The script revision used for this run did not record storage, so the JSON `machine.storage` field reads "unrecorded". The current script refuses a reference run without `--storage-description`; later reruns must pass it.

## Reference machine

| Item | Value | Source |
|------|-------|--------|
| OS | Windows 11, build 26300 (`Windows-11-10.0.26300-SP0`, AMD64) | Recorded in the JSON |
| CPU | Intel Xeon Platinum 8370C (`Intel64 Family 6 Model 106 Stepping 6`), 16 logical CPUs | Family/model and CPU count in the JSON; the model name was reported with the run, not by the script |
| RAM | 64 GiB (68,665,831,424 bytes) | Recorded in the JSON |
| Storage | `Q:` is a ReFS Dev Drive on a disk that Windows reports as a virtual SAS disk (`Msft Virtual Disk`); the physical backing is unknown | **Queried after the measurement, not captured with it.** The JSON says "unrecorded". |
| Toolchain | rustc/cargo 1.99.1 (`58cf8f9c3`, 2026-10-13, `x86_64-pc-windows-msvc`, LLVM 23.1.1); Python 3.15; Node 24.22 | rustc details recorded in the JSON; Python and Node versions reported for the environment |

Because the disk is virtual and its backing is unknown, absolute I/O-bound numbers (ingest, reopen) can shift if the host moves the volume. Compare REG-010 results only against reruns on this machine and volume, and rerun the baseline if the storage changes.

## Results (medians of 5 runs)

### 100,000 × 384 (seed 42, batch 1,000, k = 10)

| Workload | Ingest (vectors/s) | Query p50 (ms) | Query p99 (ms) | Reopen (ms) | Peak RSS (MiB) | Recall@10 |
|----------|-------------------:|---------------:|---------------:|------------:|---------------:|----------:|
| Exact (`exact_100k_384`) | 62,489 | 9.968 | 12.080 | 1,131.702 | 329.0 | 1.0 |
| Filtered exact, every 10th record eligible (`filtered_100k_384`) | 56,274 | 4.762 | 8.039 | 1,179.414 | 384.7 | 1.0 |
| HNSW, m 16, ef_construction 200, ef_search 64 (`hnsw_100k_384`) | 60,903 | 8.777 (exact) / **3.072 (HNSW)** | 11.315 (exact) / **3.630 (HNSW)** | 1,097.084 | 355.9 | 1.0 (exact) / **0.26 (HNSW)** |

HNSW index build: median 930,819.669 ms (about 15.5 minutes). The HNSW recall of 0.26 on uniformly random 384-dimension data is a property of this workload and parameters; it is recorded as the reference, not as a target.

File sizes: 155,205,232 bytes (exact and HNSW) and 157,205,232 bytes (filtered, which adds metadata).

### Existing CI `vlite-bench` workloads

| Workload | Arguments | Ingest (vectors/s) | Query p50 (ms) | Query p99 (ms) | Reopen (ms) | Peak RSS (MiB) | Recall |
|----------|-----------|-------------------:|---------------:|---------------:|------------:|---------------:|-------:|
| `exact_small` | `64 8 8 16 5 42` | 42,446 | 0.0021 | 0.0026 | 0.365 | 5.0 | 1.0 |
| `filtered_exact` | `--filter-every 10 1024 8 10 64 10 42` | 110,117 | 0.0188 | 0.0242 | 1.484 | 5.6 | 1.0 |
| `hnsw_small` | `--ann 16 100 100 256 8 10 64 5 42` | 104,447 | 0.0088 (exact) / 0.0501 (HNSW) | 0.0106 (exact) / 0.0690 (HNSW) | 0.547 | 5.1 | 1.0 / 1.0 |

`hnsw_small` index build: median 19.465 ms. Sub-millisecond timings on these small workloads are noise-dominated and are kept only as smoke references.

### Release binary sizes at `81ff901`

`vlite-bench` 1,146,368 bytes; `vlite-ffi` 2,830,336 bytes.

## Limitations

- Peak RSS is `GetProcessMemoryInfo.PeakWorkingSetSize` for the whole benchmark child process (dataset, database, exact oracle, index and queries); it is not marginal index memory and is comparable only on Windows.
- REG-010(b) open/replay time is `vlite-bench`'s `reopen_ms` of its own file; put throughput is `ingest_vectors_per_second`.
- Storage details were collected after the run (see above).
- The performance baseline is `81ff901`. Commits `62e2902`, `04014fc` and `347b01d` are newer and change the engine (HNSW selection, snapshots, group commit, maintenance). In v1.2.10 the REG-001..REG-008 fixtures were re-pinned to `347b01d` (see "Baseline re-pin" in `decisions.md`), but this performance record was not re-measured. Re-measure at `347b01d` on the reference machine, when it is otherwise idle, before REG-010 budgets are enforced or GATE-008 is requested, and then update this file.
