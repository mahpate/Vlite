# feature-agent-graph — DO-005 reference baseline (FILE-024)

Companion to [`feature-agent-graph.prd.md`](feature-agent-graph.prd.md) (DO-005, REG-010, ITEM-006) and [`decisions.md`](decisions.md). These numbers are the graph-off reference for REG-010. They are measurements, not performance guarantees; the REG-010 budgets (PD-014) are still proposals for GATE-004.

**Current baseline: release 0.6.0 (`6428b69`) on an Azure Standard_D16s_v5 VM, adopted by owner decision on 2026-10-27.** It replaces the earlier `81ff901` record from the retired Windows 11 workstation, which is kept [below](#previous-baseline-81ff901-retired) for history. Compare later REG-010 runs only against this baseline on the same VM size, image, disk and toolchain.

## What was measured

| Item | Value |
|------|-------|
| Commit | `6428b693029abbb4483ff0e96b70248d8da4c312` (tag `v0.6.0`), clean `git archive` copy, own target directory, default features, release profile |
| Paired build | `81ff901c54d674d1ad519dd4fa722e937db46c4b`, built and run the same way in the same campaign, so the old baseline commit was re-measured on the new host |
| Profile | `reference`: the CI `vlite-bench` workloads plus a seeded synthetic dataset of 100,000 records × 384 dimensions |
| Runs | 5 per workload and build; each value below is the median over the 5 runs. Revision order alternated per run (odd runs `81ff901` first, even runs `v0.6.0` first) |
| Queries | 10 per run on the 100,000 × 384 workloads (batch size 1,000, k = 10, seed 42); with 10 samples, p99 is the slowest query of a run |
| Quiet gate | Unchanged `graph_reference.py` policy for every run: background CPU mean ≤10% and no half-second sample above 50%. All 60 runs passed and exited zero |
| Raw output | [`benchmarks/results/graph-reference-v0.6.0-vs-81ff901-azure.json`](../../../benchmarks/results/graph-reference-v0.6.0-vs-81ff901-azure.json) (every run, per-run CPU samples, binary SHA-256, superseded runs); tables in the [matching `.md`](../../../benchmarks/results/graph-reference-v0.6.0-vs-81ff901-azure.md) |

**Command (per build):** `python scripts\graph_baseline.py measure --rev <rev> --profile reference --runs 5 --reference-machine-confirmed --storage-description "<disk>" --output <path>`, driven per run through `az vm run-command` with the background-CPU probe before each run.

**Two VMs.** 54 runs come from the first VM (2026-10-27 06:28–07:53 UTC). Six small-workload runs failed the CPU gate there (VM post-provisioning activity) and were each rerun once, all passing, on a second VM with the same size, image, CPU, disk and toolchain (19:42–20:13 UTC), using rebuilt binaries from the same commits. The failed originals stay in the raw file under `superseded_runs`. All 100,000 × 384 runs come from the first VM.

## Reference machine

| Item | Value |
|------|-------|
| VM | Azure Standard_D16s_v5, westus3, Standard security type; no inbound rules, driven only by run-command |
| CPU | Intel Xeon Platinum 8370C @ 2.80 GHz, 16 logical CPUs |
| RAM | 64 GiB (68,713,967,616 bytes) |
| OS | Windows Server 2022 Datacenter 10.0.20348 (image `2022-datacenter-g2:20348.5773.261025`); Defender real-time protection on, no exclusions |
| Storage | 256 GiB Premium SSD data disk (Premium_LRS, P15: 1,100 IOPS / 125 MB/s), host caching ReadOnly, NTFS `F:`; Windows reports `Msft Virtual Disk` on SAS |
| Toolchain | rustc 1.99.1 (`58cf8f9c3`, 2026-10-13), LLVM 23.1.1 |

Absolute ingest and reopen numbers depend on this disk tier. Rerun the baseline if the VM size, image, disk tier or toolchain changes.

## Results (medians of 5 runs)

### 100,000 × 384

| Workload | Ingest (vectors/s) | Query p50 (ms) | Query p99 (ms) | Reopen (ms) | Peak working set (MiB) | Recall@10 |
|----------|-------------------:|---------------:|---------------:|------------:|-----------------------:|----------:|
| Exact (`exact_100k_384`) | 220,074 | 8.540 | 8.688 | 240.078 | 326.3 | 1.0 |
| Filtered exact, every 10th record eligible (`filtered_100k_384`) | 163,754 | 4.021 | 4.306 | 298.455 | 381.9 | 1.0 |
| HNSW, m 16, ef_construction 200, ef_search 64 (`hnsw_100k_384`) | 219,562 | 8.410 (exact) / **2.140 (HNSW)** | 8.899 (exact) / **2.667 (HNSW)** | 241.309 | 388.2 | 1.0 (exact) / **0.26 (HNSW)** |

HNSW index build: median 17,953 ms. The HNSW recall of 0.26 on uniformly random 384-dimension data is a property of this workload and parameters, identical in both builds; it is recorded as the reference, not as a target.

File sizes: 155,205,232 bytes (exact and HNSW) and 157,205,232 bytes (filtered), identical in both builds.

### Existing CI `vlite-bench` workloads

| Workload | Arguments | Ingest (vectors/s) | Query p50 (ms) | Query p99 (ms) | Reopen (ms) | Peak working set (MiB) | Recall |
|----------|-----------|-------------------:|---------------:|---------------:|------------:|-----------------------:|-------:|
| `exact_small` | `64 8 8 16 5 42` | 45,153 | 0.0020 | 0.0023 | 10.847 | 3.7 | 1.0 |
| `filtered_exact` | `--filter-every 10 1024 8 10 64 10 42` | 296,803 | 0.0204 | 0.0278 | 10.588 | 4.5 | 1.0 |
| `hnsw_small` | `--ann 16 100 100 256 8 10 64 5 42` | 141,125 | 0.0060 (exact) / 0.0404 (HNSW) | 0.0065 (exact) / 0.0434 (HNSW) | 13.457 | 4.6 | 1.0 / 1.0 |

`hnsw_small` index build: median 28.957 ms. Sub-millisecond timings on these small workloads are noise-dominated and are kept only as smoke references.

### Release binary sizes at `6428b69`

`vlite-bench` 1,677,824 bytes; `vlite-ffi` 4,531,200 bytes.

## Change from `81ff901` on the same host

Same campaign, same VM, alternating order; medians.

| 100,000 × 384 | `81ff901` | `6428b69` (0.6.0) | Change |
|---|---:|---:|---:|
| Exact ingest (vectors/s) | 37,092 | 220,074 | 5.9× |
| Filtered ingest (vectors/s) | 33,183 | 163,754 | 4.9× |
| HNSW ingest (vectors/s) | 34,500 | 219,562 | 6.4× |
| Reopen, exact (ms) | 1,018 | 240 | −76% |
| Reopen, filtered (ms) | 1,078 | 298 | −72% |
| HNSW build (ms) | 796,028 | 17,953 | 44× faster |
| HNSW query p50 / p99 (ms) | 2.652 / 3.292 | 2.140 / 2.667 | −19% |
| Exact query p50 (ms) | 8.174 | 8.540 | +4.5% |
| Filtered query p50 (ms) | 3.796 | 4.021 | +5.9% |
| HNSW peak working set (MiB) | 347.0 | 388.2 | +11.9% |
| Exact / filtered peak working set (MiB) | 327.8 / 383.9 | 326.3 / 381.9 | −0.4% / −0.5% |

Binaries grew from 1,146,368 to 1,677,824 bytes (`vlite-bench`) and 2,829,824 to 4,531,200 bytes (`vlite-ffi`); graph support is compiled into every build since 0.5.1.

## Limitations

- Peak working set is the whole benchmark child process (dataset, database, exact oracle, index and queries), not marginal index memory, and is comparable only on Windows.
- REG-010(b) open/replay time is `vlite-bench`'s `reopen_ms` of its own file; put throughput is `ingest_vectors_per_second`.
- Six small-workload runs per build-pair come from a second VM of the same configuration (see above). The 100,000 × 384 results are single-VM.
- `scripts\graph_reference.py` (the graph-on NFR-002/NFR-003 runner) still checks the retired workstation's identity (`Q:` ReFS, OS build 26300). NFR-002/NFR-003 measurements must run on this baseline's VM configuration before they are compared with it, so that check has to move to this host first.

## Previous baseline: `81ff901` (retired)

Recorded in PRD v1.2.7 on the original reference workstation: Windows 11 build 26300, Xeon Platinum 8370C, 16 logical CPUs, 64 GiB, `Q:` ReFS Dev Drive on a virtual SAS disk (storage queried after the run). Five runs, same workloads and command without `--storage-description`; raw output `graph-baseline-reference-81ff901.json` (SHA-256 `0C7D5C55502560F6161AA363F24F41FC6DEDC4D61AF00BBCCD99D6F5D4F8E146`) was kept outside the repository.

| 100,000 × 384 | Ingest (vectors/s) | Query p50 / p99 (ms) | Reopen (ms) | Peak RSS (MiB) |
|---|---:|---:|---:|---:|
| Exact | 62,489 | 9.968 / 12.080 | 1,131.702 | 329.0 |
| Filtered exact | 56,274 | 4.762 / 8.039 | 1,179.414 | 384.7 |
| HNSW | 60,903 | exact 8.777 / 11.315; HNSW 3.072 / 3.630 | 1,097.084 | 355.9 |

HNSW build 930,819.669 ms; recall 0.26. These numbers are not comparable with the Azure baseline (different host and disk); the `81ff901` column in the table above is the same-host comparison.
