# DO-005 Azure benchmark campaign: release candidate `30901be1` vs v0.6.0 and v0.6.1

**Verdict: 90 of 90 slots passed** the unchanged background-CPU gate and all exited zero (0 nonzero exits). Nothing was discarded, retried or rerun: every slot of this campaign is recorded in the raw JSON.

Method: three revisions (`rc` = `30901be1732b94239e3b7fa1234593f26df63d49`, `v0.6.0` = `6428b693029abbb4483ff0e96b70248d8da4c312`, `v0.6.1` = `fb46702f6c14a6a2d9de750858cf1c14503fc563`) were built from clean `git archive` extractions with the same toolchain and measured with the `graph_baseline.py` reference profile, 5 runs per workload and revision, on four identical Azure Standard_D16s_v5 VMs (one per large workload; the three small workloads ran one after another on the fourth). The runner waited 1200 s after the builds before the first measurement. The gate, the workloads and the arguments are unchanged from the v0.6.0 baseline campaign ([baseline](../../docs/projects/feature-agent-graph/baseline.md)).

`rc` is the integration head with a green full CI run at the time of the campaign. Later release-candidate commits add server adapters, tools, clients and documentation only; the freeze section of the release notes states the exact final revision and shows that no measured path changed.

## Machine

| Item | Value |
|---|---|
| Azure region | westus3 |
| VM size | Standard_D16s_v5 (four identical VMs) |
| Image | MicrosoftWindowsServer:WindowsServer:2022-datacenter-g2:20348.5773.261025 |
| OS disk | 128 GiB Premium_LRS |
| Data disk | 256 GiB Premium_LRS, caching ReadOnly (NTFS volume F:) |
| Networking | VNet with outbound-only access; no inbound rules; driven only through `az vm run-command` |
| CPU (inside VM) | Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz |
| Logical CPUs | 16 |
| RAM bytes | 68,713,967,616 |
| OS | Microsoft Windows Server 2022 Datacenter 10.0.20348 (`Windows-2022Server-10.0.20348-SP0`) |
| Defender | {"AMRunningMode":"Normal","RealTimeProtectionEnabled":true,"AMProductVersion":"4.18.26100.4"} (untouched; no exclusions) |
| Toolchain | rustc 1.99.1 (58cf8f9c3 2026-10-13); LLVM version: 23.1.1 |
| Identity note | New Azure hosts; the historical reference-host identity checks (old Q: ReFS Dev Drive, OS build 26300) are intentionally not applied. Results compare the revisions on these same-SKU VMs only. |

## Revisions and build

| Revision | Commit | Build s per VM |
|---|---|---|
| rc | `30901be1732b94239e3b7fa1234593f26df63d49` | vm1: 161, vm2: 154, vm3: 160, vm4: 162 |
| v0.6.0 | `6428b693029abbb4483ff0e96b70248d8da4c312` | vm1: 5, vm2: 5, vm3: 4, vm4: 4 |
| v0.6.1 | `fb46702f6c14a6a2d9de750858cf1c14503fc563` | vm1: 4, vm2: 4, vm3: 4, vm4: 4 |

The `vlite-bench` and other binary SHA-256 values of every build are recorded per VM in the raw JSON (`vms`); separate builds of the same commit on different VMs are not byte-identical. The v0.6.0 and v0.6.1 builds report a few seconds because the cargo artifacts from the earlier same-day campaign on these VMs were still fresh for identical sources.

## Results (median [min-max] over 5 runs; deltas compare medians)

### exact_small (`64 8 8 16 5 42`)

| Metric | v0.6.0 | v0.6.1 | rc | rc vs v0.6.0 | v0.6.1 vs v0.6.0 |
|---|---:|---:|---:|---:|---:|
| ingest vectors/s | 32,085 [18,224-54,352] | 37,996 [16,764-42,169] | 37,031 [10,863-55,440] | +15.4% | +18.4% |
| query p50 ms | 0.002 [0.002-0.003] | 0.002 [0.002-0.002] | 0.002 [0.002-0.003] | +10.5% | +5.3% |
| query p99 ms | 0.002 [0.002-0.003] | 0.002 [0.002-0.003] | 0.002 [0.002-0.003] | +9.1% | +4.5% |
| reopen ms | 9.35 [8.97-16.70] | 10.65 [8.99-34.06] | 10.56 [9.58-55.51] | +13.0% | +13.9% |
| peak RSS MiB | 3.6 [3.6-3.6] | 3.6 [3.6-3.7] | 3.8 [3.8-4.1] | +4.8% | +0.4% |
| recall@k | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | +0.0% | +0.0% |
| file bytes | 3,312 [3,312-3,312] | 3,312 [3,312-3,312] | 3,312 [3,312-3,312] | +0.0% | +0.0% |

### filtered_exact (`--filter-every 10 1024 8 10 64 10 42`)

| Metric | v0.6.0 | v0.6.1 | rc | rc vs v0.6.0 | v0.6.1 vs v0.6.0 |
|---|---:|---:|---:|---:|---:|
| ingest vectors/s | 356,298 [200,168-359,172] | 298,403 [260,885-340,369] | 297,865 [198,862-355,025] | -16.4% | -16.2% |
| query p50 ms | 0.021 [0.020-0.021] | 0.021 [0.021-0.021] | 0.015 [0.015-0.019] | -24.8% | +1.9% |
| query p99 ms | 0.022 [0.022-0.028] | 0.026 [0.022-0.053] | 0.016 [0.016-0.020] | -27.0% | +15.3% |
| reopen ms | 11.17 [10.44-11.27] | 11.05 [9.85-12.54] | 9.95 [9.34-11.63] | -10.9% | -1.1% |
| peak RSS MiB | 4.4 [4.3-4.5] | 4.4 [4.3-4.6] | 4.6 [4.6-4.7] | +4.5% | -0.4% |
| recall@k | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | +0.0% | +0.0% |
| file bytes | 70,496 [70,496-70,496] | 70,496 [70,496-70,496] | 70,496 [70,496-70,496] | +0.0% | +0.0% |

### hnsw_small (`--ann 16 100 100 256 8 10 64 5 42`)

| Metric | v0.6.0 | v0.6.1 | rc | rc vs v0.6.0 | v0.6.1 vs v0.6.0 |
|---|---:|---:|---:|---:|---:|
| ingest vectors/s | 150,288 [125,386-174,672] | 179,096 [150,553-186,426] | 161,412 [124,109-189,321] | +7.4% | +19.2% |
| query p50 ms | 0.006 [0.006-0.007] | 0.006 [0.006-0.008] | 0.006 [0.006-0.007] | -3.3% | +0.0% |
| query p99 ms | 0.006 [0.006-0.008] | 0.006 [0.006-0.008] | 0.006 [0.006-0.008] | +1.6% | +1.6% |
| reopen ms | 16.59 [10.85-17.17] | 11.04 [9.17-22.47] | 10.55 [9.73-16.80] | -36.4% | -33.5% |
| peak RSS MiB | 4.7 [4.6-4.7] | 4.6 [4.6-4.7] | 4.8 [4.7-4.8] | +2.9% | -0.5% |
| recall@k | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | +0.0% | +0.0% |
| file bytes | 12,528 [12,528-12,528] | 12,528 [12,528-12,528] | 12,528 [12,528-12,528] | +0.0% | +0.0% |

### exact_100k_384 (`100000 384 10 1000 10 42`)

| Metric | v0.6.0 | v0.6.1 | rc | rc vs v0.6.0 | v0.6.1 vs v0.6.0 |
|---|---:|---:|---:|---:|---:|
| ingest vectors/s | 210,981 [206,294-224,958] | 211,331 [203,631-215,330] | 208,508 [207,766-218,937] | -1.2% | +0.2% |
| query p50 ms | 8.383 [7.681-8.544] | 8.771 [8.349-9.080] | 4.833 [4.482-5.128] | -42.3% | +4.6% |
| query p99 ms | 8.913 [8.681-12.432] | 10.546 [9.029-13.622] | 5.449 [4.867-7.515] | -38.9% | +18.3% |
| reopen ms | 245.96 [239.16-248.41] | 251.32 [247.43-263.55] | 248.50 [245.14-253.08] | +1.0% | +2.2% |
| peak RSS MiB | 325.3 [325.0-325.3] | 325.3 [325.0-325.6] | 325.7 [325.5-326.0] | +0.1% | +0.0% |
| recall@k | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | +0.0% | +0.0% |
| file bytes | 155,205,232 [155,205,232-155,205,232] | 155,205,232 [155,205,232-155,205,232] | 155,205,232 [155,205,232-155,205,232] | +0.0% | +0.0% |

### filtered_100k_384 (`--filter-every 10 100000 384 10 1000 10 42`)

| Metric | v0.6.0 | v0.6.1 | rc | rc vs v0.6.0 | v0.6.1 vs v0.6.0 |
|---|---:|---:|---:|---:|---:|
| ingest vectors/s | 167,202 [166,354-167,682] | 166,576 [166,159-169,309] | 165,459 [163,074-166,459] | -1.0% | -0.4% |
| query p50 ms | 3.631 [3.581-3.647] | 3.566 [3.545-3.698] | 2.782 [2.768-2.816] | -23.4% | -1.8% |
| query p99 ms | 3.854 [3.822-3.927] | 3.843 [3.756-3.849] | 2.964 [2.841-3.079] | -23.1% | -0.3% |
| reopen ms | 295.24 [278.74-297.75] | 294.23 [281.82-298.15] | 297.24 [280.09-298.48] | +0.7% | -0.3% |
| peak RSS MiB | 381.9 [381.6-381.9] | 381.9 [381.6-381.9] | 381.9 [381.9-382.2] | +0.0% | +0.0% |
| recall@k | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | +0.0% | +0.0% |
| file bytes | 157,205,232 [157,205,232-157,205,232] | 157,205,232 [157,205,232-157,205,232] | 157,205,232 [157,205,232-157,205,232] | +0.0% | +0.0% |

### hnsw_100k_384 (`--ann 16 200 64 100000 384 10 1000 10 42`)

| Metric | v0.6.0 | v0.6.1 | rc | rc vs v0.6.0 | v0.6.1 vs v0.6.0 |
|---|---:|---:|---:|---:|---:|
| ingest vectors/s | 212,699 [209,708-216,390] | 211,527 [205,436-215,829] | 218,014 [213,822-219,646] | +2.5% | -0.6% |
| query p50 ms | 8.208 [8.115-8.338] | 8.319 [8.196-8.423] | 4.715 [4.539-4.906] | -42.6% | +1.3% |
| query p99 ms | 8.775 [8.351-9.885] | 8.734 [8.495-13.117] | 5.135 [4.904-5.717] | -41.5% | -0.5% |
| reopen ms | 246.93 [234.48-250.18] | 248.21 [234.57-252.71] | 250.66 [246.27-268.20] | +1.5% | +0.5% |
| peak RSS MiB | 389.1 [388.1-390.5] | 388.6 [388.1-389.5] | 390.2 [389.2-391.6] | +0.3% | -0.1% |
| recall@k | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | 1.000 [1.000-1.000] | +0.0% | +0.0% |
| file bytes | 155,205,232 [155,205,232-155,205,232] | 155,205,232 [155,205,232-155,205,232] | 155,205,232 [155,205,232-155,205,232] | +0.0% | +0.0% |

Differences on the three small workloads (runs of 0.1-0.2 s) are within the run-to-run spread; the `filtered_exact` ingest difference already exists between v0.6.0 and v0.6.1 and is unchanged between v0.6.1 and rc.

## Background CPU (fraction of 16 logical CPUs; gate: mean at most 10%, no half-second sample above 50%)

| Workload | Revision | Slots passed | Max of per-run mean | Max sample |
|---|---|---:|---:|---:|
| exact_small | rc | 5/5 | 5.75% | 5.75% |
| exact_small | v0.6.0 | 5/5 | 5.53% | 5.53% |
| exact_small | v0.6.1 | 5/5 | 6.40% | 5.64% |
| filtered_exact | rc | 5/5 | 5.39% | 0.00% |
| filtered_exact | v0.6.0 | 5/5 | 9.56% | 0.00% |
| filtered_exact | v0.6.1 | 5/5 | 6.01% | 0.00% |
| hnsw_small | rc | 5/5 | 3.67% | 3.67% |
| hnsw_small | v0.6.0 | 5/5 | 4.08% | 0.00% |
| hnsw_small | v0.6.1 | 5/5 | 7.29% | 0.00% |
| exact_100k_384 | rc | 5/5 | 0.67% | 1.54% |
| exact_100k_384 | v0.6.0 | 5/5 | 0.36% | 1.35% |
| exact_100k_384 | v0.6.1 | 5/5 | 0.46% | 1.47% |
| filtered_100k_384 | rc | 5/5 | 0.61% | 1.63% |
| filtered_100k_384 | v0.6.0 | 5/5 | 0.35% | 1.33% |
| filtered_100k_384 | v0.6.1 | 5/5 | 0.32% | 1.22% |
| hnsw_100k_384 | rc | 5/5 | 0.11% | 1.89% |
| hnsw_100k_384 | v0.6.0 | 5/5 | 0.10% | 2.17% |
| hnsw_100k_384 | v0.6.1 | 5/5 | 0.09% | 1.39% |

## Gate tally

| Workload | v0.6.0 | v0.6.1 | rc |
|---|---:|---:|---:|
| exact_small | 5/5 | 5/5 | 5/5 |
| filtered_exact | 5/5 | 5/5 | 5/5 |
| hnsw_small | 5/5 | 5/5 | 5/5 |
| exact_100k_384 | 5/5 | 5/5 | 5/5 |
| filtered_100k_384 | 5/5 | 5/5 | 5/5 |
| hnsw_100k_384 | 5/5 | 5/5 | 5/5 |

## Protocol

- Gate (unchanged `scripts/graph_reference.py`): mean background at most 10% of logical CPUs and no 0.5 s sample above 50%, for the preflight (`calm_preflight(idle_probe)`, up to 12 five-second probes) and for the run (`run_process`). Background = system busy CPU minus `vlite-bench` process CPU.
- Workloads and arguments: `graph_baseline.py` SMOKE_WORKLOADS and REFERENCE_WORKLOADS (reference profile), 5 runs each; the revision order rotates with the run number.
- Builds: `git archive` extraction per revision, `cargo fetch --locked`, then `graph_baseline.build_release` (`--offline --locked --release`, default features) into separate target directories.
- Settle: 1200 s after the builds and before the first measurement (the baseline campaign used 180 s); this is a wait only, the thresholds are unchanged.
- No run was discarded or retried. A failed preflight would still have run the benchmark and been recorded as failed; none failed.
- The four VMs and their cloud resources were created only for this campaign and are removed after the evidence is preserved; Defender was untouched.
