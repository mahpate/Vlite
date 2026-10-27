# DO-005 Azure benchmark campaign: v0.6.0 vs 81ff901 (combined: original campaign + selective rerun)

**Verdict (combined): 60 of 60 slots passed** the unchanged background-CPU gate and exited zero (60 slots recorded). 54 slots come from the original campaign on VM 1; 6 come from a selective rerun of the original campaign's failed slots on a second VM of the same SKU and CPU (the rerun needed 6 attempts in total for 6 slots).

Method: Combined DO-005 Azure result: 54 slots from the original campaign (VM 1, Standard_D16s_v5, westus3, 2026-10-27T06:28Z–07:53Z) plus 6 slots selectively rerun on a second VM of the same SKU and CPU (Standard_D16s_v5, westus3, Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz, 2026-10-27T19:42Z–20:13Z). Only the 6 original slots that failed the background-CPU gate were rerun, with identical binaries built the same way, identical gb.SMOKE_WORKLOADS arguments and the unchanged graph_reference.py gate. Each rerun slot could be retried at most 3 more times; every attempt is recorded in rerun\ and here, and the first passing attempt replaces the failed original slot, which is kept under the workload's superseded_runs.

Every slot now has a passing measurement, but 6 of them were measured on a second VM after the original campaign; whether a combined two-VM result counts as DO-005 baseline evidence is a policy decision, not something this summary decides.

## Machine

| Item | Value |
|---|---|
| Azure region | westus3 |
| VM size | Standard_D16s_v5 |
| Image | MicrosoftWindowsServer:WindowsServer:2022-datacenter-g2:20348.5773.261025 |
| OS disk | 128 GiB Premium_LRS |
| Data disk | 256 GiB Premium_LRS, caching ReadOnly |
| Security type | Standard |
| VM created (UTC) | 2026-10-27T06:03:31Z |
| Networking | VNet + Standard static public IP for outbound; NSG with zero custom inbound rules; driven only via az vm run-command |
| CPU (inside VM) | Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz |
| Logical CPUs | 16 |
| RAM bytes | 68,713,967,616 |
| OS | Microsoft Windows Server 2022 Datacenter 10.0.20348 (`Windows-2022Server-10.0.20348-SP0`) |
| Volume | {"DriveLetter":"F","FileSystem":"NTFS","Size":274860077056} |
| Disk (Windows view) | {"FriendlyName":"Msft Virtual Disk","BusType":"SAS","Size":274877906944} |
| Storage description | Azure managed data disk, Premium SSD Premium_LRS 256 GiB (P15 tier: 1,100 IOPS / 125 MB/s baseline), host caching ReadOnly, NTFS volume F: (Windows reports Msft Virtual Disk, BusType SAS); VM Standard_D16s_v5 in westus3 |
| Defender | {"AMRunningMode":"Normal","RealTimeProtectionEnabled":true,"AMProductVersion":"4.18.26090.9"} (untouched; no exclusions) |
| Toolchain | rustc 1.99.1 (58cf8f9c3 2026-10-13); LLVM version: 23.1.1 |
| Identity note | New Azure host; graph_reference.py reference-host identity checks (old Q: ReFS Dev Drive, OS 10.0.26300, 68,665,831,424 bytes RAM) are intentionally not applied. Identity recorded here. |

## Rerun machine (second VM, 6 slots)

| Item | Value |
|---|---|
| Azure region | westus3 |
| VM size | Standard_D16s_v5 |
| Image | MicrosoftWindowsServer:WindowsServer:2022-datacenter-g2:20348.5773.261025 |
| OS disk | 128 GiB Premium_LRS |
| Data disk | 256 GiB Premium_LRS, caching ReadOnly |
| Security type | Standard |
| VM created (UTC) | 2026-10-27T19:24:49Z |
| Networking | VNet + Standard static public IP for outbound; NSG with zero custom inbound rules; driven only via az vm run-command |
| Resource group | vlite-do005-rerun-20261027 |
| CPU create attempts | 1 |
| CPU (inside VM) | Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz |
| Logical CPUs | 16 |
| RAM bytes | 68,713,967,616 |
| OS | Microsoft Windows Server 2022 Datacenter 10.0.20348 (`Windows-2022Server-10.0.20348-SP0`) |
| Volume | {"DriveLetter":"F","FileSystem":"NTFS","Size":274860077056} |
| Disk (Windows view) | {"FriendlyName":"Msft Virtual Disk","BusType":"SAS","Size":274877906944} |
| Defender | {"AMRunningMode":"Normal","RealTimeProtectionEnabled":true,"AMProductVersion":"4.18.26080.4"} (untouched; no exclusions) |
| Toolchain | rustc 1.99.1 (58cf8f9c3 2026-10-13); LLVM version: 23.1.1 |

| Revision | Rerun build s | Same commit | vlite-bench sha256 matches VM 1 build |
|---|---:|---|---|
| 81ff901 | 110 | True | False |
| v0.6.0 | 139 | True | False |

## Revisions and build

| Revision | Commit | Build s | vlite-bench bytes | vlite-ffi bytes | REG-013 |
|---|---|---:|---:|---:|---|
| 81ff901 | `81ff901c54d674d1ad519dd4fa722e937db46c4b` | 112 | 1,146,368 | 2,829,824 | not present at this revision |
| v0.6.0 | `6428b693029abbb4483ff0e96b70248d8da4c312` | 139 | 1,677,824 | 4,531,200 | see JSON |

Order: per run number, per workload; revision order alternates (odd runs 81ff901 first, even runs v0.6.0 first). Original campaign started 2026-10-27T06:28:40.961637+00:00, finished 2026-10-27T07:53:07.848173+00:00. Selective rerun started 2026-10-27T19:42:44.417000+00:00, finished 2026-10-27T20:13:14.971604+00:00.

## Results (median [min–max] over runs; Δ = v0.6.0 vs 81ff901 median)

### exact_small (`64 8 8 16 5 42`)

| Metric | 81ff901 | v0.6.0 | Δ |
|---|---|---|---:|
| ingest vec/s | 4,694 [1,820–6,155] (n=5) | 45,153 [29,556–49,740] (n=5) | +861.9% |
| query p50 ms | 0.0019 [0.0018–0.0025] (n=5) | 0.0020 [0.0020–0.0020] (n=5) | +5.3% |
| query p99 ms | 0.0027 [0.0022–0.0031] (n=5) | 0.0023 [0.0022–0.0024] (n=5) | -14.8% |
| reopen ms | 2.559 [1.225–8.458] (n=5) | 10.847 [6.273–11.798] (n=5) | +323.9% |
| peak WS MiB | 3.570 [3.562–3.828] (n=5) | 3.664 [3.656–3.934] (n=5) | +2.6% |
| recall@k | 1.000 [1.000–1.000] (n=5) | 1.000 [1.000–1.000] (n=5) | +0.0% |
| elapsed s | 0.0470 [0.0411–0.1794] (n=5) | 0.0523 [0.0500–0.1604] (n=5) | +11.5% |

### filtered_exact (`--filter-every 10 1024 8 10 64 10 42`)

| Metric | 81ff901 | v0.6.0 | Δ |
|---|---|---|---:|
| ingest vec/s | 17,262 [14,357–21,194] (n=5) | 296,803 [89,630–322,865] (n=5) | +1619.4% |
| query p50 ms | 0.0236 [0.0190–0.0238] (n=5) | 0.0204 [0.0203–0.0258] (n=5) | -13.6% |
| query p99 ms | 0.0243 [0.0194–0.0248] (n=5) | 0.0278 [0.0262–0.0402] (n=5) | +14.4% |
| reopen ms | 11.568 [10.352–12.013] (n=5) | 10.588 [7.093–10.807] (n=5) | -8.5% |
| peak WS MiB | 4.289 [4.285–4.387] (n=5) | 4.504 [4.367–4.508] (n=5) | +5.0% |
| recall@k | 1.000 [1.000–1.000] (n=5) | 1.000 [1.000–1.000] (n=5) | +0.0% |
| elapsed s | 0.1044 [0.0891–0.1144] (n=5) | 0.0648 [0.0610–0.1537] (n=5) | -37.9% |

### hnsw_small (`--ann 16 100 100 256 8 10 64 5 42`)

| Metric | 81ff901 | v0.6.0 | Δ |
|---|---|---|---:|
| ingest vec/s | 17,648 [10,879–19,758] (n=5) | 141,125 [134,390–181,367] (n=5) | +699.7% |
| query p50 ms | 0.0057 [0.0056–0.0061] (n=5) | 0.0060 [0.0060–0.0074] (n=5) | +5.3% |
| query p99 ms | 0.0063 [0.0060–0.0084] (n=5) | 0.0065 [0.0062–0.0079] (n=5) | +3.2% |
| reopen ms | 9.950 [7.559–10.899] (n=5) | 13.457 [7.162–16.474] (n=5) | +35.2% |
| peak WS MiB | 3.719 [3.703–3.793] (n=5) | 4.641 [4.609–4.770] (n=5) | +24.8% |
| recall@k | 1.000 [1.000–1.000] (n=5) | 1.000 [1.000–1.000] (n=5) | +0.0% |
| HNSW build ms | 15.065 [14.989–15.177] (n=5) | 28.957 [28.488–29.533] (n=5) | +92.2% |
| HNSW p50 ms | 0.0368 [0.0365–0.0375] (n=5) | 0.0404 [0.0397–0.0414] (n=5) | +9.8% |
| HNSW p99 ms | 0.0383 [0.0380–0.0410] (n=5) | 0.0434 [0.0420–0.0513] (n=5) | +13.3% |
| HNSW recall | 1.000 [1.000–1.000] (n=5) | 1.000 [1.000–1.000] (n=5) | +0.0% |
| elapsed s | 0.0691 [0.0678–0.0811] (n=5) | 0.0879 [0.0858–0.0934] (n=5) | +27.3% |

### exact_100k_384 (`100000 384 10 1000 10 42`)

| Metric | 81ff901 | v0.6.0 | Δ |
|---|---|---|---:|
| ingest vec/s | 37,092 [24,803–39,578] (n=5) | 220,074 [215,646–222,332] (n=5) | +493.3% |
| query p50 ms | 8.174 [8.066–8.277] (n=5) | 8.540 [8.310–8.577] (n=5) | +4.5% |
| query p99 ms | 8.433 [8.275–9.032] (n=5) | 8.688 [8.626–8.975] (n=5) | +3.0% |
| reopen ms | 1,018 [1,012–1,051] (n=5) | 240.078 [234.428–242.538] (n=5) | -76.4% |
| peak WS MiB | 327.773 [327.219–328.285] (n=5) | 326.328 [325.309–326.340] (n=5) | -0.4% |
| recall@k | 1.000 [1.000–1.000] (n=5) | 1.000 [1.000–1.000] (n=5) | +0.0% |
| elapsed s | 4.806 [4.643–6.130] (n=5) | 2.665 [2.529–3.050] (n=5) | -44.6% |

### filtered_100k_384 (`--filter-every 10 100000 384 10 1000 10 42`)

| Metric | 81ff901 | v0.6.0 | Δ |
|---|---|---|---:|
| ingest vec/s | 33,183 [28,581–39,981] (n=5) | 163,754 [163,042–164,132] (n=5) | +393.5% |
| query p50 ms | 3.796 [3.719–3.905] (n=5) | 4.021 [3.951–4.134] (n=5) | +5.9% |
| query p99 ms | 4.202 [4.005–5.585] (n=5) | 4.306 [4.167–4.895] (n=5) | +2.5% |
| reopen ms | 1,078 [1,069–1,095] (n=5) | 298.455 [285.299–310.098] (n=5) | -72.3% |
| peak WS MiB | 383.859 [383.855–383.988] (n=5) | 381.906 [380.633–381.918] (n=5) | -0.5% |
| recall@k | 1.000 [1.000–1.000] (n=5) | 1.000 [1.000–1.000] (n=5) | +0.0% |
| elapsed s | 5.199 [4.667–5.661] (n=5) | 2.954 [2.871–3.140] (n=5) | -43.2% |

### hnsw_100k_384 (`--ann 16 200 64 100000 384 10 1000 10 42`)

| Metric | 81ff901 | v0.6.0 | Δ |
|---|---|---|---:|
| ingest vec/s | 34,500 [32,203–38,016] (n=5) | 219,562 [216,234–219,796] (n=5) | +536.4% |
| query p50 ms | 8.108 [7.735–8.260] (n=5) | 8.410 [8.360–8.486] (n=5) | +3.7% |
| query p99 ms | 8.546 [7.984–8.890] (n=5) | 8.899 [8.727–10.428] (n=5) | +4.1% |
| reopen ms | 1,018 [1,016–1,021] (n=5) | 241.309 [237.533–251.833] (n=5) | -76.3% |
| peak WS MiB | 346.977 [346.918–347.023] (n=5) | 388.184 [387.367–389.426] (n=5) | +11.9% |
| recall@k | 1.000 [1.000–1.000] (n=5) | 1.000 [1.000–1.000] (n=5) | +0.0% |
| HNSW build ms | 796,028 [792,109–799,601] (n=5) | 17,953 [17,778–18,640] (n=5) | -97.7% |
| HNSW p50 ms | 2.652 [2.615–2.726] (n=5) | 2.140 [2.128–2.183] (n=5) | -19.3% |
| HNSW p99 ms | 3.292 [3.243–3.395] (n=5) | 2.667 [2.577–2.704] (n=5) | -19.0% |
| HNSW recall | 0.2600 [0.2600–0.2600] (n=5) | 0.2600 [0.2600–0.2600] (n=5) | +0.0% |
| elapsed s | 801.454 [797.481–805.463] (n=5) | 21.486 [20.930–21.913] (n=5) | -97.3% |

## Per-run background CPU (fraction of 16 logical CPUs)

| # | Run | Workload | Revision | Source | rc | Elapsed s | Preflight probes | Preflight mean / worst | Run mean / worst | Gate |
|---:|---:|---|---|---|---:|---:|---:|---|---|---|
| 1 | 1 | exact_small | 81ff901 | rerun VM 2, attempt 1 | 0 | 0.1794 | 1 | 0.02% / 0.2% | 5.45% / 5.4% | pass |
| 2 | 1 | exact_small | v0.6.0 | rerun VM 2, attempt 1 | 0 | 0.1604 | 1 | 0.08% / 0.8% | 5.48% / 5.5% | pass |
| 3 | 1 | filtered_exact | 81ff901 | original VM 1 | 0 | 0.1059 | 1 | 6.71% / 10.0% | 9.23% / 9.2% | pass |
| 4 | 1 | filtered_exact | v0.6.0 | rerun VM 2, attempt 1 | 0 | 0.1537 | 1 | 0.02% / 0.2% | 1.27% / 1.3% | pass |
| 5 | 1 | hnsw_small | 81ff901 | original VM 1 | 0 | 0.0811 | 1 | 6.64% / 8.2% | 9.65% / 0.0% | pass |
| 6 | 1 | hnsw_small | v0.6.0 | rerun VM 2, attempt 1 | 0 | 0.0868 | 1 | 0.49% / 2.7% | 3.38% / 0.0% | pass |
| 7 | 1 | exact_100k_384 | 81ff901 | original VM 1 | 0 | 4.749 | 1 | 6.34% / 7.0% | 6.56% / 7.6% | pass |
| 8 | 1 | exact_100k_384 | v0.6.0 | original VM 1 | 0 | 2.529 | 1 | 6.46% / 7.8% | 6.68% / 7.6% | pass |
| 9 | 1 | filtered_100k_384 | 81ff901 | original VM 1 | 0 | 4.667 | 1 | 6.50% / 7.4% | 6.45% / 7.4% | pass |
| 10 | 1 | filtered_100k_384 | v0.6.0 | original VM 1 | 0 | 2.934 | 1 | 6.52% / 8.6% | 6.42% / 6.8% | pass |
| 11 | 1 | hnsw_100k_384 | 81ff901 | original VM 1 | 0 | 801.454 | 1 | 4.29% / 8.2% | 6.43% / 37.9% | pass |
| 12 | 1 | hnsw_100k_384 | v0.6.0 | original VM 1 | 0 | 21.913 | 1 | 6.09% / 7.6% | 5.42% / 7.8% | pass |
| 13 | 2 | exact_small | v0.6.0 | original VM 1 | 0 | 0.0523 | 1 | 6.42% / 7.6% | 5.62% / 0.0% | pass |
| 14 | 2 | exact_small | 81ff901 | original VM 1 | 0 | 0.0470 | 1 | 6.21% / 7.8% | 6.26% / 0.0% | pass |
| 15 | 2 | filtered_exact | v0.6.0 | original VM 1 | 0 | 0.0610 | 1 | 6.75% / 7.6% | 8.03% / 0.0% | pass |
| 16 | 2 | filtered_exact | 81ff901 | original VM 1 | 0 | 0.0891 | 1 | 6.28% / 6.8% | 7.69% / 0.0% | pass |
| 17 | 2 | hnsw_small | v0.6.0 | rerun VM 2, attempt 1 | 0 | 0.0900 | 1 | 0.06% / 0.2% | 2.17% / 0.0% | pass |
| 18 | 2 | hnsw_small | 81ff901 | rerun VM 2, attempt 1 | 0 | 0.0691 | 1 | 1.91% / 5.3% | 2.83% / 0.0% | pass |
| 19 | 2 | exact_100k_384 | v0.6.0 | original VM 1 | 0 | 2.665 | 1 | 6.79% / 8.8% | 6.56% / 7.2% | pass |
| 20 | 2 | exact_100k_384 | 81ff901 | original VM 1 | 0 | 4.643 | 1 | 6.62% / 7.8% | 6.88% / 8.0% | pass |
| 21 | 2 | filtered_100k_384 | v0.6.0 | original VM 1 | 0 | 2.871 | 1 | 6.44% / 7.6% | 6.73% / 8.1% | pass |
| 22 | 2 | filtered_100k_384 | 81ff901 | original VM 1 | 0 | 4.822 | 1 | 6.56% / 7.8% | 6.36% / 7.8% | pass |
| 23 | 2 | hnsw_100k_384 | v0.6.0 | original VM 1 | 0 | 21.768 | 1 | 6.87% / 8.6% | 5.30% / 6.6% | pass |
| 24 | 2 | hnsw_100k_384 | 81ff901 | original VM 1 | 0 | 797.481 | 1 | 6.95% / 8.6% | 0.14% / 20.3% | pass |
| 25 | 3 | exact_small | 81ff901 | original VM 1 | 0 | 0.0473 | 1 | 0.39% / 2.9% | 2.08% / 0.0% | pass |
| 26 | 3 | exact_small | v0.6.0 | original VM 1 | 0 | 0.0500 | 1 | 5.66% / 18.5% | 5.88% / 0.0% | pass |
| 27 | 3 | filtered_exact | 81ff901 | original VM 1 | 0 | 0.0900 | 1 | 0.02% / 0.2% | 2.17% / 0.0% | pass |
| 28 | 3 | filtered_exact | v0.6.0 | original VM 1 | 0 | 0.0648 | 1 | 0.02% / 0.2% | 3.02% / 0.0% | pass |
| 29 | 3 | hnsw_small | 81ff901 | original VM 1 | 0 | 0.0678 | 1 | 0.00% / 0.0% | 2.90% / 0.0% | pass |
| 30 | 3 | hnsw_small | v0.6.0 | original VM 1 | 0 | 0.0934 | 1 | 0.10% / 0.6% | 2.09% / 0.0% | pass |
| 31 | 3 | exact_100k_384 | 81ff901 | original VM 1 | 0 | 4.806 | 1 | 0.02% / 0.2% | 0.06% / 0.2% | pass |
| 32 | 3 | exact_100k_384 | v0.6.0 | original VM 1 | 0 | 2.661 | 1 | 0.16% / 1.6% | 0.22% / 1.0% | pass |
| 33 | 3 | filtered_100k_384 | 81ff901 | original VM 1 | 0 | 5.199 | 1 | 0.12% / 1.0% | 0.15% / 0.8% | pass |
| 34 | 3 | filtered_100k_384 | v0.6.0 | original VM 1 | 0 | 2.954 | 1 | 0.16% / 1.6% | 0.10% / 0.2% | pass |
| 35 | 3 | hnsw_100k_384 | 81ff901 | original VM 1 | 0 | 802.344 | 1 | 0.18% / 1.6% | 0.06% / 17.6% | pass |
| 36 | 3 | hnsw_100k_384 | v0.6.0 | original VM 1 | 0 | 21.486 | 1 | 0.12% / 1.0% | 0.05% / 0.6% | pass |
| 37 | 4 | exact_small | v0.6.0 | original VM 1 | 0 | 0.0515 | 1 | 0.12% / 0.6% | 5.71% / 0.0% | pass |
| 38 | 4 | exact_small | 81ff901 | original VM 1 | 0 | 0.0463 | 1 | 0.04% / 0.2% | 2.12% / 0.0% | pass |
| 39 | 4 | filtered_exact | v0.6.0 | original VM 1 | 0 | 0.0726 | 1 | 0.04% / 0.2% | 2.70% / 0.0% | pass |
| 40 | 4 | filtered_exact | 81ff901 | original VM 1 | 0 | 0.1044 | 1 | 0.10% / 0.8% | 1.87% / 1.9% | pass |
| 41 | 4 | hnsw_small | v0.6.0 | original VM 1 | 0 | 0.0858 | 1 | 0.00% / 0.0% | 4.56% / 0.0% | pass |
| 42 | 4 | hnsw_small | 81ff901 | original VM 1 | 0 | 0.0803 | 1 | 0.02% / 0.2% | 2.45% / 0.0% | pass |
| 43 | 4 | exact_100k_384 | v0.6.0 | original VM 1 | 0 | 3.050 | 1 | 0.02% / 0.2% | 0.06% / 0.2% | pass |
| 44 | 4 | exact_100k_384 | 81ff901 | original VM 1 | 0 | 5.321 | 1 | 0.10% / 1.0% | 0.13% / 0.9% | pass |
| 45 | 4 | filtered_100k_384 | v0.6.0 | original VM 1 | 0 | 3.055 | 1 | 0.14% / 1.0% | 0.16% / 0.4% | pass |
| 46 | 4 | filtered_100k_384 | 81ff901 | original VM 1 | 0 | 5.446 | 1 | 0.10% / 1.0% | 0.13% / 1.0% | pass |
| 47 | 4 | hnsw_100k_384 | v0.6.0 | original VM 1 | 0 | 21.246 | 1 | 0.10% / 1.0% | 0.04% / 0.5% | pass |
| 48 | 4 | hnsw_100k_384 | 81ff901 | original VM 1 | 0 | 800.484 | 1 | 0.12% / 1.2% | 0.06% / 16.1% | pass |
| 49 | 5 | exact_small | 81ff901 | original VM 1 | 0 | 0.0411 | 1 | 0.10% / 1.0% | 4.78% / 0.0% | pass |
| 50 | 5 | exact_small | v0.6.0 | original VM 1 | 0 | 0.0543 | 1 | 0.02% / 0.2% | 5.42% / 0.0% | pass |
| 51 | 5 | filtered_exact | 81ff901 | original VM 1 | 0 | 0.1144 | 1 | 0.04% / 0.2% | 1.71% / 1.7% | pass |
| 52 | 5 | filtered_exact | v0.6.0 | original VM 1 | 0 | 0.0614 | 1 | 0.02% / 0.2% | 1.59% / 0.0% | pass |
| 53 | 5 | hnsw_small | 81ff901 | original VM 1 | 0 | 0.0683 | 1 | 0.00% / 0.0% | 1.44% / 0.0% | pass |
| 54 | 5 | hnsw_small | v0.6.0 | original VM 1 | 0 | 0.0879 | 1 | 0.02% / 0.2% | 4.45% / 0.0% | pass |
| 55 | 5 | exact_100k_384 | 81ff901 | original VM 1 | 0 | 6.130 | 1 | 0.00% / 0.0% | 0.13% / 0.6% | pass |
| 56 | 5 | exact_100k_384 | v0.6.0 | original VM 1 | 0 | 2.704 | 1 | 0.12% / 1.2% | 0.29% / 0.8% | pass |
| 57 | 5 | filtered_100k_384 | 81ff901 | original VM 1 | 0 | 5.661 | 1 | 0.10% / 0.8% | 0.12% / 0.6% | pass |
| 58 | 5 | filtered_100k_384 | v0.6.0 | original VM 1 | 0 | 3.140 | 1 | 0.14% / 1.4% | 0.34% / 6.8% | pass |
| 59 | 5 | hnsw_100k_384 | 81ff901 | original VM 1 | 0 | 805.463 | 1 | 0.02% / 0.2% | 0.09% / 24.2% | pass |
| 60 | 5 | hnsw_100k_384 | v0.6.0 | original VM 1 | 0 | 20.930 | 1 | 0.12% / 1.0% | 0.05% / 2.4% | pass |

## Selective rerun: every attempt (second VM)

| # | Run | Workload | Revision | Source | rc | Elapsed s | Preflight probes | Preflight mean / worst | Run mean / worst | Gate |
|---:|---:|---|---|---|---:|---:|---:|---|---|---|
| 1 | 1 | exact_small | 81ff901 | attempt 1 (used) | 0 | 0.1794 | 1 | 0.02% / 0.2% | 5.45% / 5.4% | pass |
| 2 | 1 | exact_small | v0.6.0 | attempt 1 (used) | 0 | 0.1604 | 1 | 0.08% / 0.8% | 5.48% / 5.5% | pass |
| 3 | 1 | filtered_exact | v0.6.0 | attempt 1 (used) | 0 | 0.1537 | 1 | 0.02% / 0.2% | 1.27% / 1.3% | pass |
| 4 | 1 | hnsw_small | v0.6.0 | attempt 1 (used) | 0 | 0.0868 | 1 | 0.49% / 2.7% | 3.38% / 0.0% | pass |
| 5 | 2 | hnsw_small | v0.6.0 | attempt 1 (used) | 0 | 0.0900 | 1 | 0.06% / 0.2% | 2.17% / 0.0% | pass |
| 6 | 2 | hnsw_small | 81ff901 | attempt 1 (used) | 0 | 0.0691 | 1 | 1.91% / 5.3% | 2.83% / 0.0% | pass |

## Superseded original slots (VM 1, kept in `superseded_runs`)

| # | Run | Workload | Revision | Source | rc | Elapsed s | Preflight probes | Preflight mean / worst | Run mean / worst | Gate |
|---:|---:|---|---|---|---:|---:|---:|---|---|---|
| 1 | 1 | exact_small | 81ff901 | original VM 1 → replaced by rerun attempt 1 | 0 | 0.1681 | 1 | 6.73% / 7.8% | 11.05% / 11.1% | **FAIL** |
| 2 | 2 | hnsw_small | 81ff901 | original VM 1 → replaced by rerun attempt 1 | 0 | 0.0697 | 1 | 6.83% / 8.0% | 18.26% / 0.0% | **FAIL** |
| 3 | 1 | exact_small | v0.6.0 | original VM 1 → replaced by rerun attempt 1 | 0 | 0.1694 | 1 | 6.89% / 9.0% | 12.69% / 12.7% | **FAIL** |
| 4 | 1 | filtered_exact | v0.6.0 | original VM 1 → replaced by rerun attempt 1 | 0 | 0.0607 | 1 | 7.22% / 9.8% | 14.51% / 0.0% | **FAIL** |
| 5 | 1 | hnsw_small | v0.6.0 | original VM 1 → replaced by rerun attempt 1 | 0 | 0.0855 | 1 | 6.46% / 6.8% | 12.59% / 0.0% | **FAIL** |
| 6 | 2 | hnsw_small | v0.6.0 | original VM 1 → replaced by rerun attempt 1 | 0 | 0.0871 | 1 | 7.03% / 8.2% | 10.12% / 0.0% | **FAIL** |

## Gate tally

| Workload | 81ff901 passed | v0.6.0 passed |
|---|---:|---:|
| exact_small | 5/5 | 5/5 |
| filtered_exact | 5/5 | 5/5 |
| hnsw_small | 5/5 | 5/5 |
| exact_100k_384 | 5/5 | 5/5 |
| filtered_100k_384 | 5/5 | 5/5 |
| hnsw_100k_384 | 5/5 | 5/5 |

## Protocol

- Gate (unchanged `scripts/graph_reference.py`): mean background ≤ 10% of logical CPUs and no 0.5 s sample > 50%, for the preflight (`calm_preflight(idle_probe)`, up to 12 five-second probes) and for the run (`run_process`). Background = system busy CPU minus vlite-bench process CPU.
- Workloads/arguments: `graph_baseline.py` SMOKE_WORKLOADS + REFERENCE_WORKLOADS (reference profile), 5 runs each; vlite-bench launched directly under `run_process` (peak working set of the vlite-bench process, as `tests/measure_benchmark.py` records).
- Builds: `git archive` extraction per revision, `cargo fetch --locked`, then `graph_baseline.build_release` (`--offline --locked --release`, default features) into separate target dirs.
- Original campaign: no run was discarded or retried; 54 of 60 slots passed. A failed preflight still ran the benchmark, and the run was recorded as failed.
- Selective rerun: only the 6 failed slots were rerun on a second VM of the same SKU and CPU, with both revisions rebuilt the same way, the same arguments and the same unchanged gate. After the 180 s post-build settle the runner also waited for post-provisioning activity to subside (after the 180s settle and before each attempt: wait until 6 consecutive graph_reference.idle_probe windows each show mean background <= 2% and no CPU use by TiWorker, TrustedInstaller, msiexec, MpCmdRun, MsMpEng, MoUsoCoreWorker, wuauclt, CompatTelRunner, WaAppAgent, WindowsAzureGuestAgent, ngen, mscorsvw, SearchIndexer (TiWorker/MpCmdRun must not be running); cap 3600s first, 600s before retries. Wait criterion only; gate thresholds unchanged; Defender untouched, no exclusions). Each slot could be retried at most 3 more times; all attempts are listed above and the first passing attempt is used. The replaced original slots are kept in each workload's `superseded_runs`.
- Not the historical DO-005 reference host (different OS build and disk); results compare the two revisions on these same-SKU VMs only.
