# Supplemental 0.7.0 measurements on Azure: directory store, declared indexes, 768 dimensions and queries during writes

The [release-candidate profile](graph-reference-0.7.0-rc-vs-v0.6.0-azure.md) measures default-path exact, filtered and HNSW search. It does not exercise the opt-in directory store, declared metadata indexes, 768-dimensional exact search or queries during writes. This supplement measures them on the same machine configuration (four Azure Standard_D16s_v5 VMs in westus3, Windows Server 2022, Premium SSD data disk; see the machine table of that profile), with the same method: every revision is built from a clean `git archive` copy (release profile, Rust 1.99.1, `--locked --offline`), every `vlite-bench` process runs inside the unchanged `graph_reference.py` gate (background CPU mean at most 10%, no half-second sample above 50%), the revision order rotates per run, and nothing is discarded or retried. Five runs per workload and revision.

## Campaigns and revisions

| Campaign | VMs | Revisions | Settle after builds | Workloads |
|---|---|---|---:|---|
| `supp` | vm1-vm4 | candidate `2225a33391dec972a44d6639d69ae2ee37ff678d`; v0.6.0 `6428b693029abbb4483ff0e96b70248d8da4c312` and v0.6.1 `fb46702f6c14a6a2d9de750858cf1c14503fc563` where the workload runs on them | 1,200 s | directory-store reopen at 100,000 x 384, clustered filters, queries during writes, 768-dimensional exact search (the last was replaced by `supp2`) |
| `supp2` | vm1, vm2 | candidate `2225a333`, v0.6.0, v0.6.1 | 600 s | exact search at 80,000 x 768; reopen at 400,000 x 96 and 1,000,000 x 32 |
| `supp3` | vm3 | candidate `2225a333`, v0.6.0, v0.6.1 | 300 s | the reference filtered workload (10% of records, spread, no index) on all three revisions |
| `supp4` | vm4 | candidate `b68fcfa881d57c828c9a19b5a9662f4b948db9f8` | 300 s | the four metadata index choices on three match shapes |
| `supp5` | vm1, vm2 | candidate `b68fcfa8` (`cand`) and `58b1819442c317a5e4bd33bb5db45a42f5837564` (`fix`) | 300 s | the same index workloads, both revisions on one VM |

The candidate `2225a333` is the release-candidate integration head at that time; `b68fcfa8` adds only the `--metadata-index` kind argument to `vlite-bench`; `58b18194` adds the filter-summaries early out described below. Neither later revision changes a code path used by the default workloads.

## Slot accounting

435 slots produced a measurement. 432 passed the gate, 3 failed it and none exited with a nonzero status. The three failures are in `supp5`: one slot on vm1 (background CPU mean 10.89%) and two on vm2 (10.95% and 10.17%). Two coincide with the harness's remote progress polls of the VM; the cause of the third was not established. They are recorded in the raw JSON with `passed: false`, excluded from the medians (each affected cell keeps at least three passed runs) and were not retried.

Another 35 slots, five workloads (`exact_100k_768` and four `reopen_*_1m_384` workloads), exceed `vlite-bench`'s 256 MiB data-set cap and exited at once without a measurement. They were replaced by `exact_80k_768`, `reopen_*_1m_32` and `reopen_*_400k_96`, which fit the cap, and are listed in the JSON under `workloads_without_measurement`.

Absolute times differ between VMs by up to 70% (the 10% spread filter without an index reads 2.97 ms on vm1 and vm4 and 5.0 ms on vm2), so every comparison below is made within one VM and one campaign.

[Raw slots](supplement-0.7.0-azure.json)

## Reopen: single file against directory stores

| Scale | Layout | Slots | Reopen ms | First query ms | Checkpoint ms | Storage MiB | Query p50 ms | Ingest vectors/s |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| 100,000 x 384 | file | 5 | 237.98 [233.58-239.94] (baseline) | 4.77 [4.56-4.83] | - | 148.0 [148.0-148.0] | 4.354 [4.284-4.517] | 223,061 [218,511-225,353] |
| 100,000 x 384 | directory (checkpointed) | 5 | 170.18 [169.03-172.08] (-28.5%) | 3.58 [3.33-3.74] | 2,677 [2,536-2,961] | 150.0 [150.0-150.0] | 3.668 [3.564-3.701] | 116,527 [114,060-117,927] |
| 100,000 x 384 | directory-auto | 5 | 206.02 [203.45-208.47] (-13.4%) | 3.87 [3.85-4.00] | - | 148.9 [148.9-148.9] | 4.041 [3.940-4.163] | 108,665 [101,241-118,777] |
| 100,000 x 384 | directory-log | 5 | 238.62 [236.67-243.78] (+0.3%) | 4.75 [4.51-5.62] | - | 148.0 [148.0-148.0] | 4.311 [4.248-4.313] | 222,961 [222,441-225,195] |
| 400,000 x 96 | file | 5 | 341.02 [337.49-342.91] (baseline) | 10.44 [10.29-10.51] | - | 152.6 [152.6-152.6] | 10.070 [10.010-10.207] | 734,666 [732,176-745,582] |
| 400,000 x 96 | directory (checkpointed) | 5 | 242.69 [237.40-246.07] (-28.8%) | 7.11 [6.93-7.17] | 3,518 [3,115-3,590] | 160.6 [160.6-160.6] | 7.209 [7.090-7.356] | 378,743 [366,841-396,899] |
| 400,000 x 96 | directory-auto | 5 | 304.22 [302.16-305.02] (-10.8%) | 8.64 [8.53-8.78] | - | 156.0 [156.0-156.0] | 8.880 [8.828-8.922] | 355,789 [349,352-440,115] |
| 400,000 x 96 | directory-log | 5 | 341.13 [336.64-342.93] (+0.0%) | 10.43 [10.16-10.60] | - | 152.6 [152.6-152.6] | 10.105 [9.975-10.164] | 733,742 [729,790-743,153] |
| 1,000,000 x 32 | file | 5 | 575.63 [564.49-664.20] (baseline) | 37.35 [34.20-37.75] | - | 137.4 [137.4-137.4] | 34.053 [33.376-36.215] | 1,074,639 [870,309-1,095,426] |
| 1,000,000 x 32 | directory (checkpointed) | 5 | 512.48 [491.49-549.09] (-11.0%) | 21.12 [19.15-22.35] | 3,956 [3,667-4,376] | 157.4 [157.4-157.4] | 20.332 [19.580-21.661] | 667,292 [649,979-695,321] |
| 1,000,000 x 32 | directory-auto | 5 | 562.58 [542.51-605.37] (-2.3%) | 30.30 [27.95-38.79] | - | 146.7 [146.7-146.7] | 28.687 [27.776-28.825] | 644,230 [621,450-679,709] |
| 1,000,000 x 32 | directory-log | 5 | 568.13 [563.64-601.14] (-1.3%) | 36.53 [34.23-38.89] | - | 137.4 [137.4-137.4] | 34.290 [33.054-38.479] | 1,079,819 [1,039,113-1,091,204] |

## Exact search at 768 dimensions (80,000 vectors)

| Revision | Slots | Query p50 ms | Query p99 ms | Ingest vectors/s | Reopen ms | Peak working set MiB |
|---|---:|---:|---:|---:|---:|---:|
| v0.6.0 | 5 | 10.617 [10.446-10.621] | 10.754 [10.661-10.917] | 124,106 [123,296-124,709] | 365.28 [347.70-369.39] | 498.2 [498.2-498.2] |
| v0.6.1 | 5 | 10.502 [10.463-10.574] | 11.123 [10.792-11.403] | 124,748 [123,214-126,255] | 362.25 [352.60-373.32] | 498.4 [498.2-498.4] |
| cand | 5 | 4.897 [4.815-4.992] | 5.049 [4.969-5.283] | 124,327 [121,756-125,138] | 370.11 [353.61-378.76] | 498.5 [498.3-498.5] |

Candidate against v0.6.0: query p50 -53.9%, p99 -53.0%, ingest +0.2%, reopen +1.3%.

## Filtered exact search, first campaign (candidate `2225a333`, vm3, before the filter-summaries fix)

The spread row with a range index predates the fix. The same-VM comparison in the next section supersedes it.

| Workload | Slots | Eligible | Records scanned p50 | Query p50 ms | Query p99 ms | vs no index (same data) |
|---|---:|---:|---:|---:|---:|---:|
| 10% clustered, no index | 5 | 10,240 | - | 4.122 [3.987-4.256] | 4.647 [4.247-6.548] | baseline |
| 10% clustered, range index | 5 | 10,240 | 10,240 [10,240-10,240] | 2.627 [2.495-2.675] | 2.933 [2.840-3.340] | -36.3% |
| 1% clustered, no index | 5 | 1,024 | - | 3.661 [3.596-3.707] | 4.251 [3.823-5.322] | baseline |
| 1% clustered, range index | 5 | 1,024 | 1,024 [1,024-1,024] | 0.513 [0.510-0.519] | 0.568 [0.527-0.714] | -86.0% |
| 10% spread, no index (v0.6.0) | 5 | 10,000 | - | 5.159 [5.040-5.234] | 5.531 [5.240-5.897] | baseline |
| 10% spread, no index (v0.6.1) | 5 | 10,000 | - | 5.226 [5.099-5.283] | 5.970 [5.659-7.866] | baseline |
| 10% spread, no index (candidate) | 5 | 10,000 | - | 4.052 [4.005-4.094] | 4.787 [4.312-5.971] | baseline |
| 10% spread, range index (candidate) | 5 | 10,000 | 100,000 [100,000-100,000] | 8.316 [8.190-8.502] | 9.100 [9.053-9.148] | +105.2% |

## Queries during writes: lazy rebuild against background rebuild (20,000 x 384, 16 writes and 4 deletes before each of 30 queries)

| Mode | Pacing ms | Slots | Served / attempted | Served recall@10 | Request p50 ms | Request p99 ms | Request max ms | Rebuilds in request path |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| lazy | 0 | 5 | 150/150 (100%) | 0.53 [0.53-0.53] | 1.98 [1.96-2.03] | 1,649.8 [1,634.2-1,665.3] | 1,649.8 [1,634.2-1,665.3] | 2 per run |
| lazy | 250 | 5 | 150/150 (100%) | 0.53 [0.53-0.53] | 2.62 [2.57-2.66] | 1,657.0 [1,648.3-1,973.1] | 1,657.0 [1,648.3-1,973.1] | 2 per run |
| background | 0 | 5 | 60/150 (40%) | 0.57 [0.57-0.57] | 0.37 [0.25-0.99] | 4.3 [4.0-7.2] | 4.3 [4.0-7.2] | 0 (background worker) |
| background | 250 | 5 | 150/150 (100%) | 0.54 [0.54-0.54] | 2.66 [2.57-2.79] | 14.6 [14.5-21.6] | 14.6 [14.5-21.6] | 0 (background worker) |

## Declared metadata indexes: the same data, queries and binary (100,000 x 384 vectors, 10 queries, k = 10)

`none` declares no index; `eq` keeps equality postings; `range` keeps ordered postings for integers and floats and the block summaries (the benchmark field is a boolean, so for it `range` adds only the summaries); `eq-range` keeps both kinds. `cand` is `b68fcfa8`; `fix` is `58b18194`, which stops a range index that cannot prune from building a candidate set before it scans everything. "Matches in whole blocks" means the matching records fill whole 1,024-record blocks (`--filter-block 1024`); "spread" means every tenth record matches.

Slots: 300; passed the unchanged background-CPU gate: 297; nonzero exits: 0. Worst per-slot background CPU mean 10.95% (limit 10%); worst half-second sample 26.90% (limit 50%).

### vm1: `cand` = `b68fcfa8`, `fix` = `58b18194`

| Matches | Index | Eligible | cand scanned p50 | cand query p50 ms | cand vs no index | fix scanned p50 | fix query p50 ms | fix vs no index | fix vs cand |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 10% of records, spread | no index | 10,000 | - | 2.966 [2.942-3.105] | baseline | - | 2.972 [2.916-2.998] | baseline | +0.2% |
| 10% of records, spread | eq | 10,000 | 10,000 [10,000-10,000] | 2.589 [2.516-2.630] | -12.7% | 10,000 [10,000-10,000] | 2.524 [2.496-2.655] | -15.1% | -2.5% |
| 10% of records, spread | range | 10,000 | 100,000 [100,000-100,000] | 7.039 [6.911-7.099] | +137.3% | 100,000 [100,000-100,000] | 3.000 [2.850-3.081] | +1.0% | -57.4% |
| 10% of records, spread | eq-range | 10,000 | 10,000 [10,000-10,000] | 2.523 [2.488-2.547] | -14.9% | 10,000 [10,000-10,000] | 2.579 [2.526-2.590] | -13.2% | +2.2% |
| 10% in whole blocks | no index | 10,240 | - | 2.996 [2.960-3.052] | baseline | - | 2.999 [2.907-3.025] | baseline | +0.1% |
| 10% in whole blocks | eq | 10,240 | 10,240 [10,240-10,240] | 1.742 [1.695-1.801] | -41.9% | 10,240 [10,240-10,240] | 1.733 [1.659-1.743] | -42.2% | -0.5% |
| 10% in whole blocks | range | 10,240 | 10,240 [10,240-10,240] | 2.246 [2.213-2.253] | -25.0% | 10,240 [10,240-10,240] | 2.215 [2.158-2.338] | -26.1% | -1.4% |
| 10% in whole blocks | eq-range | 10,240 | 10,240 [10,240-10,240] | 1.823 [1.701-1.894] | -39.1% | 10,240 [10,240-10,240] | 1.766 [1.676-1.831] | -41.1% | -3.1% |
| 1% in whole blocks | no index | 1,024 | - | 2.709 [2.633-2.844] | baseline | - | 2.648 [2.604-2.722] | baseline | -2.2% |
| 1% in whole blocks | eq | 1,024 | 1,024 [1,024-1,024] | 0.351 [0.348-0.355] | -87.0% | 1,024 [1,024-1,024] | 0.340 [0.336-0.353] | -87.2% | -3.3% |
| 1% in whole blocks | range | 1,024 | 1,024 [1,024-1,024] | 0.398 [0.382-0.406] | -85.3% | 1,024 [1,024-1,024] | 0.392 [0.389-0.403] | -85.2% | -1.4% |
| 1% in whole blocks | eq-range | 1,024 | 1,024 [1,024-1,024] | 0.352 [0.339-0.360] | -87.0% | 1,024 [1,024-1,024] | 0.342 [0.334-0.347] | -87.1% | -2.9% |

### vm2: `cand` = `b68fcfa8`, `fix` = `58b18194`

| Matches | Index | Eligible | cand scanned p50 | cand query p50 ms | cand vs no index | fix scanned p50 | fix query p50 ms | fix vs no index | fix vs cand |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 10% of records, spread | no index | 10,000 | - | 4.998 [4.854-5.345] | baseline | - | 5.253 [4.745-5.992] | baseline | +5.1% |
| 10% of records, spread | eq | 10,000 | 10,000 [10,000-10,000] | 3.949 [3.804-4.427] | -21.0% | 10,000 [10,000-10,000] | 3.841 [3.734-4.257] | -26.9% | -2.7% |
| 10% of records, spread | range | 10,000 | 100,000 [100,000-100,000] | 9.342 [9.136-9.964] | +86.9% | 100,000 [100,000-100,000] | 5.205 [4.579-5.339] | -0.9% | -44.3% |
| 10% of records, spread | eq-range | 10,000 | 10,000 [10,000-10,000] | 4.020 [3.758-4.195] | -19.6% | 10,000 [10,000-10,000] | 3.917 [3.738-4.202] | -25.4% | -2.6% |
| 10% in whole blocks | no index | 10,240 | - | 5.032 [4.536-5.322] | baseline | - | 5.228 [4.725-5.502] | baseline | +3.9% |
| 10% in whole blocks | eq | 10,240 | 10,240 [10,240-10,240] | 2.440 [2.354-2.573] | -51.5% | 10,240 [10,240-10,240] | 2.486 [2.429-2.615] | -52.4% | +1.9% |
| 10% in whole blocks | range | 10,240 | 10,240 [10,240-10,240] | 3.003 [2.788-3.061] | -40.3% | 10,240 [10,240-10,240] | 2.906 [2.761-3.105] | -44.4% | -3.2% |
| 10% in whole blocks | eq-range | 10,240 | 10,240 [10,240-10,240] | 2.532 [2.463-2.563] | -49.7% | 10,240 [10,240-10,240] | 2.583 [2.353-2.728] | -50.6% | +2.0% |
| 1% in whole blocks | no index | 1,024 | - | 4.841 [4.162-5.188] | baseline | - | 4.891 [4.061-5.173] | baseline | +1.0% |
| 1% in whole blocks | eq | 1,024 | 1,024 [1,024-1,024] | 0.543 [0.496-0.712] | -88.8% | 1,024 [1,024-1,024] | 0.542 [0.491-0.591] | -88.9% | -0.1% |
| 1% in whole blocks | range | 1,024 | 1,024 [1,024-1,024] | 0.576 [0.559-0.659] | -88.1% | 1,024 [1,024-1,024] | 0.569 [0.562-0.573] | -88.4% | -1.3% |
| 1% in whole blocks | eq-range | 1,024 | 1,024 [1,024-1,024] | 0.516 [0.508-0.595] | -89.3% | 1,024 [1,024-1,024] | 0.563 [0.520-0.583] | -88.5% | +9.2% |

### vm4: `cand` = `b68fcfa8`

| Matches | Index | Eligible | cand scanned p50 | cand query p50 ms | cand vs no index |
|---|---|---:|---:|---:|---:|
| 10% of records, spread | no index | 10,000 | - | 2.974 [2.916-3.088] | baseline |
| 10% of records, spread | eq | 10,000 | 10,000 [10,000-10,000] | 2.579 [2.504-2.664] | -13.3% |
| 10% of records, spread | range | 10,000 | 100,000 [100,000-100,000] | 7.093 [6.951-7.150] | +138.5% |
| 10% of records, spread | eq-range | 10,000 | 10,000 [10,000-10,000] | 2.542 [2.460-2.610] | -14.5% |
| 10% in whole blocks | no index | 10,240 | - | 2.935 [2.882-3.012] | baseline |
| 10% in whole blocks | eq | 10,240 | 10,240 [10,240-10,240] | 1.732 [1.627-1.827] | -41.0% |
| 10% in whole blocks | range | 10,240 | 10,240 [10,240-10,240] | 2.221 [2.144-2.264] | -24.3% |
| 10% in whole blocks | eq-range | 10,240 | 10,240 [10,240-10,240] | 1.722 [1.646-1.833] | -41.3% |
| 1% in whole blocks | no index | 1,024 | - | 2.668 [2.661-2.740] | baseline |
| 1% in whole blocks | eq | 1,024 | 1,024 [1,024-1,024] | 0.324 [0.320-0.350] | -87.9% |
| 1% in whole blocks | range | 1,024 | 1,024 [1,024-1,024] | 0.384 [0.370-0.393] | -85.6% |
| 1% in whole blocks | eq-range | 1,024 | 1,024 [1,024-1,024] | 0.337 [0.330-0.339] | -87.4% |
