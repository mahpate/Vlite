# DO-005 graph-off benchmark — separate-host comparison report

Campaign `Q:\vlite-campaign\do005-sephost-20261025T172902Z` (tag `main`), generated 2026-10-26T01:18:40.429+00:00; measurements 2026-10-25 18:30:03 – 2026-10-26 00:59:01 UTC.

> **Status: INCOMPLETE** — 58 of 60 (workload × revision × repetition) slots have an accepted run (58 attempts rejected, 0 refused, 0 slots not attempted).
> **Separate host** `[REDACTED-HOSTNAME]` — AMD EPYC 9V74 80-Core Processor, 16 logical CPUs. This report does not replace, amend or approve the original host's baseline; no gate is evaluated or approved; nothing was committed or pushed; baseline documents are untouched.
> **CPU acceptance policy unchanged**: background CPU mean ≤ 10 %, every sample of at least 0.1 s ≤ 50 %, scanner/System CPU counted as background, measured by the pinned `scripts/graph_reference.py` monitor.
> Slots without an accepted run are **missing evidence**, not zero or interpolated values; see "Missing evidence".

## 1. Outcome

| Workload | hist accepted slots | hist attempts a/r/f/n | cur accepted slots | cur attempts a/r/f/n |
| :-- | --: | --: | --: | --: |
| `exact_small` | 5/5 | 5/6/0/0 | 5/5 | 5/5/0/0 |
| `filtered_exact` | 5/5 | 5/6/0/0 | 5/5 | 5/5/0/0 |
| `hnsw_small` | 5/5 | 5/8/0/0 | 5/5 | 5/11/0/0 |
| `exact_100k_384` | 5/5 | 5/1/0/0 | 5/5 | 5/0/0/0 |
| `filtered_100k_384` | 5/5 | 5/1/0/0 | 5/5 | 5/0/0/0 |
| `hnsw_100k_384` | 4/5 | 4/7/0/0 | 4/5 | 4/8/0/0 |


a/r/f/n = accepted / rejected (contaminated or failed) / refused (no quiet pre-run probe) / not attempted; +Nq = N runs the runner accepted but this report quarantined (a monitor sample longer than 5 s, section 4). Retries are bounded per slot (operational time bounds, not quality thresholds): `exact_small` 15, `filtered_exact` 15, `hnsw_small` 15, `exact_100k_384` 8, `filtered_100k_384` 8, `hnsw_100k_384` 3; a workload stops being attempted after 2 slots exhausted their attempts with nothing accepted (or 4 exhausted slots overall); the campaign stops after 3 consecutive refused pre-run probes.

### Missing evidence
- `hnsw_100k_384/hist`: repetitions [2] have no accepted run — 1 slot(s) used all 3 allowed attempts without passing the CPU policy
- `hnsw_100k_384/cur`: repetitions [3] have no accepted run — 1 slot(s) used all 3 allowed attempts without passing the CPU policy

## 2. Results — historical `81ff901` vs current `origin/main`
Statistics use **accepted runs only** (median, min–max, n). Δ is (cur median − hist median) / hist median. With n ≤ 5 no significance is claimed; "ranges overlap" only says the two observed ranges intersect. Higher is better for throughput and recall; lower is better for times and memory.

### `exact_small`

| Metric | Unit | hist: median (min–max) | cur: median (min–max) | Δ medians | ranges |
| :-- | :-- | --: | --: | --: | :-: |
| Ingest | vectors/s | 53,108 (43,472–57,281) n=5 | 49,601 (45,374–57,637) n=5 | -6.6% | overlap |
| Exact query p50 | ms | 0.0018 (0.0018–0.0030) n=5 | 0.0020 (0.0020–0.0030) n=5 | +11.1% | overlap |
| Exact query p99 | ms | 0.0025 (0.0023–0.0066) n=5 | 0.0026 (0.0025–0.0039) n=5 | +4.0% | overlap |
| Reopen | ms | 0.1870 (0.1775–0.1934) n=5 | 0.1599 (0.1507–0.2088) n=5 | -14.5% | overlap |
| Peak working set | MiB | 4.625 (4.621–4.855) n=5 | 4.688 (4.680–4.914) n=5 | +1.4% | overlap |
| Exact recall@k | – | 1 (1–1) n=5 | 1 (1–1) n=5 | +0.0% | overlap |
| Child wall time | s | 0.0280 (0.0267–0.0346) n=5 | 0.0278 (0.0161–0.1825) n=5 | -0.7% | overlap |


### `filtered_exact`

| Metric | Unit | hist: median (min–max) | cur: median (min–max) | Δ medians | ranges |
| :-- | :-- | --: | --: | --: | :-: |
| Ingest | vectors/s | 143,135 (69,428–146,751) n=5 | 149,561 (143,982–155,173) n=5 | +4.5% | overlap |
| Exact query p50 | ms | 0.0196 (0.0193–0.0203) n=5 | 0.0228 (0.0223–0.0238) n=5 | +16.3% | disjoint |
| Exact query p99 | ms | 0.0213 (0.0205–0.0523) n=5 | 0.0267 (0.0247–0.0477) n=5 | +25.4% | overlap |
| Reopen | ms | 1.415 (1.331–1.463) n=5 | 0.9912 (0.9811–1.059) n=5 | -30.0% | disjoint |
| Peak working set | MiB | 5.195 (5.188–5.422) n=5 | 5.273 (5.258–5.496) n=5 | +1.5% | overlap |
| Exact recall@k | – | 1 (1–1) n=5 | 1 (1–1) n=5 | +0.0% | overlap |
| Child wall time | s | 0.0396 (0.0357–0.0458) n=5 | 0.0256 (0.0240–0.0357) n=5 | -35.4% | overlap |


### `hnsw_small`

| Metric | Unit | hist: median (min–max) | cur: median (min–max) | Δ medians | ranges |
| :-- | :-- | --: | --: | --: | :-: |
| Ingest | vectors/s | 156,088 (148,578–164,514) n=5 | 159,770 (142,825–164,071) n=5 | +2.4% | overlap |
| Exact query p50 | ms | 0.0054 (0.0053–0.0054) n=5 | 0.0058 (0.0058–0.0059) n=5 | +7.4% | disjoint |
| Exact query p99 | ms | 0.0058 (0.0058–0.0067) n=5 | 0.0065 (0.0063–0.0066) n=5 | +12.1% | overlap |
| Reopen | ms | 0.2824 (0.2696–0.3294) n=5 | 0.2322 (0.2295–0.2649) n=5 | -17.8% | disjoint |
| Peak working set | MiB | 4.762 (4.758–4.988) n=5 | 4.773 (4.766–5.000) n=5 | +0.2% | overlap |
| Exact recall@k | – | 1 (1–1) n=5 | 1 (1–1) n=5 | +0.0% | overlap |
| HNSW index build | ms | 15.778 (15.533–15.804) n=5 | 13.997 (13.819–14.044) n=5 | -11.3% | disjoint |
| HNSW query p50 | ms | 0.0402 (0.0400–0.0410) n=5 | 0.0416 (0.0409–0.0426) n=5 | +3.5% | overlap |
| HNSW query p99 | ms | 0.0483 (0.0420–0.0530) n=5 | 0.0612 (0.0430–0.0791) n=5 | +26.7% | overlap |
| HNSW recall@k | – | 1 (1–1) n=5 | 1 (1–1) n=5 | +0.0% | overlap |
| Child wall time | s | 0.0456 (0.0451–0.0496) n=5 | 0.0363 (0.0317–0.0465) n=5 | -20.4% | overlap |


### `exact_100k_384`

| Metric | Unit | hist: median (min–max) | cur: median (min–max) | Δ medians | ranges |
| :-- | :-- | --: | --: | --: | :-: |
| Ingest | vectors/s | 66,710 (61,485–67,593) n=5 | 156,248 (153,324–169,151) n=5 | +134.2% | disjoint |
| Exact query p50 | ms | 8.936 (8.870–9.447) n=5 | 8.719 (8.406–9.023) n=5 | -2.4% | overlap |
| Exact query p99 | ms | 9.930 (9.334–13.229) n=5 | 9.946 (9.352–10.973) n=5 | +0.2% | overlap |
| Reopen | ms | 1,172 (1,156–1,202) n=5 | 231.6 (218.0–236.3) n=5 | -80.2% | disjoint |
| Peak working set | MiB | 328.2 (328.2–328.5) n=5 | 325.9 (325.8–326.0) n=5 | -0.7% | disjoint |
| Exact recall@k | – | 1 (1–1) n=5 | 1 (1–1) n=5 | +0.0% | overlap |
| Child wall time | s | 3.795 (3.752–3.944) n=5 | 2.014 (1.943–2.043) n=5 | -46.9% | disjoint |


### `filtered_100k_384`

| Metric | Unit | hist: median (min–max) | cur: median (min–max) | Δ medians | ranges |
| :-- | :-- | --: | --: | --: | :-: |
| Ingest | vectors/s | 59,904 (52,401–61,710) n=5 | 124,206 (117,197–125,881) n=5 | +107.3% | disjoint |
| Exact query p50 | ms | 5.532 (5.253–5.548) n=5 | 5.628 (5.263–5.813) n=5 | +1.7% | overlap |
| Exact query p99 | ms | 5.850 (5.406–7.047) n=5 | 6.169 (5.675–8.161) n=5 | +5.5% | overlap |
| Reopen | ms | 1,221 (1,213–1,233) n=5 | 279.4 (266.7–285.4) n=5 | -77.1% | disjoint |
| Peak working set | MiB | 384.8 (384.8–385.0) n=5 | 382.0 (382.0–382.3) n=5 | -0.7% | disjoint |
| Exact recall@k | – | 1 (1–1) n=5 | 1 (1–1) n=5 | +0.0% | overlap |
| Child wall time | s | 4.017 (3.951–4.307) n=5 | 2.223 (2.198–2.273) n=5 | -44.7% | disjoint |


### `hnsw_100k_384`

| Metric | Unit | hist: median (min–max) | cur: median (min–max) | Δ medians | ranges |
| :-- | :-- | --: | --: | --: | :-: |
| Ingest | vectors/s | 66,210 (65,566–66,829) n=4 | 161,514 (155,570–166,767) n=4 | +143.9% | disjoint |
| Exact query p50 | ms | 8.991 (8.275–9.216) n=4 | 8.729 (8.277–8.994) n=4 | -2.9% | overlap |
| Exact query p99 | ms | 9.962 (9.443–10.480) n=4 | 9.796 (9.529–10.540) n=4 | -1.7% | overlap |
| Reopen | ms | 1,167 (1,165–1,175) n=4 | 229.8 (221.9–232.4) n=4 | -80.3% | disjoint |
| Peak working set | MiB | 355.7 (355.5–355.8) n=4 | 353.3 (353.2–353.4) n=4 | -0.7% | disjoint |
| Exact recall@k | – | 1 (1–1) n=4 | 1 (1–1) n=4 | +0.0% | overlap |
| HNSW index build | ms | 776,356 (754,832–804,562) n=4 | 774,224 (757,428–795,119) n=4 | -0.3% | overlap |
| HNSW query p50 | ms | 3.056 (2.835–3.113) n=4 | 3.178 (3.116–3.288) n=4 | +4.0% | disjoint |
| HNSW query p99 | ms | 3.697 (3.465–3.893) n=4 | 3.960 (3.724–4.037) n=4 | +7.1% | overlap |
| HNSW recall@k | – | 0.26 (0.26–0.26) n=4 | 0.26 (0.26–0.26) n=4 | +0.0% | overlap |
| Child wall time | s | 780.7 (759.2–809.0) n=4 | 776.8 (759.9–797.7) n=4 | -0.5% | overlap |


Revision order inside each accepted pair alternates by design; the historical revision ran first in 14 pairs and the current one in 14 pairs.

## 3. CPU-policy outcome of the measured runs
Background CPU is system busy time minus the benchmark's own CPU, sampled every 0.5 s by the pinned monitor; the limits are 10 % (mean over the run) and 50 % (any sample of at least 0.1 s).

| Workload/revision | accepted | median mean bg | max mean bg | max sample bg | peak check vacuous | runs < 1 s | median wall (s) | longest monitor sample (s) |
| :-- | --: | --: | --: | --: | --: | --: | --: | --: |
| `exact_small/hist` | 5 | 6.62% | 7.38% | n/a (vacuous) | 5 | 5 | 0.0280 | 0.03 |
| `exact_small/cur` | 5 | 7.10% | 9.55% | 9.11% | 4 | 5 | 0.0278 | 0.18 |
| `filtered_exact/hist` | 5 | 6.44% | 8.27% | n/a (vacuous) | 5 | 5 | 0.0396 | 0.05 |
| `filtered_exact/cur` | 5 | 3.90% | 8.21% | n/a (vacuous) | 5 | 5 | 0.0256 | 0.04 |
| `hnsw_small/hist` | 5 | 6.46% | 8.68% | n/a (vacuous) | 5 | 5 | 0.0456 | 0.05 |
| `hnsw_small/cur` | 5 | 8.41% | 9.32% | n/a (vacuous) | 5 | 5 | 0.0363 | 0.05 |
| `exact_100k_384/hist` | 5 | 5.92% | 7.10% | 15.61% | 0 | 0 | 3.795 | 0.52 |
| `exact_100k_384/cur` | 5 | 4.32% | 7.02% | 9.55% | 0 | 0 | 2.014 | 0.52 |
| `filtered_100k_384/hist` | 5 | 5.59% | 9.90% | 20.93% | 0 | 0 | 4.017 | 0.53 |
| `filtered_100k_384/cur` | 5 | 2.77% | 9.16% | 19.75% | 0 | 0 | 2.223 | 0.52 |
| `hnsw_100k_384/hist` | 4 | 5.95% | 8.06% | 49.22% | 0 | 0 | 780.7 | 0.55 |
| `hnsw_100k_384/cur` | 4 | 6.73% | 7.39% | 47.08% | 0 | 0 | 776.8 | 0.52 |


"Peak check vacuous": the run was shorter than 0.1 s, so the monitor's single final sample is not counted for the peak rule and acceptance rests on the mean alone. "Longest monitor sample" is the report's stalled-sample check (a run is quarantined above 5 s; nominal sample length 0.5 s); for the sub-second workloads it is the run's own length.

Where the background CPU went (share of all logical CPUs over the measured child runs; top entries of each run's process census, so a lower bound):
- accepted runs (58 runs, 6303 s): GlobalSecureAccessClient.exe 1.19%, msedgewebview2.exe 0.97%, MsMpEng.exe 0.63%, svchost.exe 0.60%, System 0.52%, vmmemWSL 0.34%
- rejected runs (58 runs, 16318 s): GlobalSecureAccessClient.exe 0.87%, MsMpEng.exe 0.73%, svchost.exe 0.53%, msedgewebview2.exe 0.44%, System 0.42%, vmmemWSL 0.25%

Why attempts were rejected by the unchanged limits:

| Workload | rejected | only the 50 % sample limit | only the 10 % mean limit | both | other (not CPU) |
| :-- | --: | --: | --: | --: | --: |
| `exact_small` | 11 | 0 | 11 | 0 | 0 |
| `filtered_exact` | 11 | 0 | 11 | 0 | 0 |
| `hnsw_small` | 19 | 0 | 19 | 0 | 0 |
| `exact_100k_384` | 1 | 0 | 1 | 0 | 0 |
| `filtered_100k_384` | 1 | 0 | 1 | 0 | 0 |
| `hnsw_100k_384` | 15 | 15 | 0 | 0 | 0 |


## 4. Quiet-window evidence
**Pre-window gate**: PASSED — up to 6 passive surveys of 180 s under the unchanged policy; 2 used.

| Survey | UTC | s | mean bg | worst sample | policy met | 5 s probes quiet | top contributors (share of all CPUs) |
| :-- | :-- | --: | --: | --: | :-: | --: | :-- |
| pre-1 | 18:23:02 | 180 | 9.64% | 99.82% | NO | 28/36 | MsMpEng.exe 2.03%, GlobalSecureAccessClient.exe 1.29%, svchost.exe 0.95%, msedgewebview2.exe 0.82% |
| pre-2 | 18:27:03 | 180 | 5.03% | 33.77% | yes | 33/36 | GlobalSecureAccessClient.exe 1.18%, MsMpEng.exe 0.66%, System 0.41%, vmmemWSL 0.30% |
| post | 00:59:01 | 180 | 6.23% | 26.54% | yes | 35/36 | msedgewebview2.exe 1.41%, GlobalSecureAccessClient.exe 1.09%, vmmemWSL 0.54%, System 0.49% |


**Exploratory long surveys** (not gating; taken while the host was otherwise unused, to characterise its floor):
- `explore-1`: 120 s, mean 11.89%, worst sample 26.93%, policy met: False, 5 s probes quiet 9/24; per-minute means: 9.4, 14.3 %; top: SenseCE.exe 4.44%, svchost.exe 1.42%, GlobalSecureAccessClient.exe 1.09%, WmiPrvSE.exe 1.00%, System 0.55%, MsMpEng.exe 0.48%
- `explore-2`: 599 s, mean 12.03%, worst sample 63.25%, policy met: False, 5 s probes quiet 52/119; per-minute means: 18.9, 9.4, 10.7, 8.4, 10.0, 9.6, 9.4, 10.9, 15.0, 18.2 %; top: SenseCE.exe 4.43%, svchost.exe 1.21%, GlobalSecureAccessClient.exe 1.11%, WmiPrvSE.exe 0.63%, MsMpEng.exe 0.59%, System 0.55%

**Process censuses and other app sessions:**
- Process census at campaign start (69 watched processes of 19 kinds): blocking build/benchmark processes: none.
- Process census at campaign end (57 watched processes of 16 kinds): blocking build/benchmark processes: none.
- Watched-process differences between the start and end census: copilot-language-server.exe 1→0, copilot.exe 1→0, devenv.exe 1→0, git.exe 5→6, msedge.exe 15→7, node.exe 6→4. (Servicing processes such as `TiWorker.exe`/`TrustedInstaller.exe` appearing mid-campaign would be a contamination source; full censuses are in `raw/<tag>/census-*.json`.)
- Other app sessions (pre, 2026-10-25T18:22:34+00:00): 0 busy of 5 (this session excluded); all other app sessions idle, none awaiting input or plan approval. The app itself, other desktop applications and the user's own activity cannot be controlled and are counted as background CPU by the monitor
- Other app sessions (post, 2026-10-26T01:02:56+00:00): 0 busy of 5 (this session excluded); all other app sessions were idle at both snapshots (pre 18:22:34Z, post 01:02:56Z), none awaiting input or plan approval. No snapshot was taken between these two instants, so the state of the other sessions during the 6.6 h window is inferred, not observed. The app itself, other desktop applications and the user's own activity cannot be controlled and are counted as background CPU by the monitor

**Quiet-window status: met**, by a rule fixed before the campaign: it is `met` only if the pre-window survey passed the unchanged policy, no build/benchmark processes were running at the start, and no other app session was busy before the start; `NOT MET` if the pre-window survey never passed; otherwise `partial`. The post survey and the end-of-campaign checks are reported but do not decide the status (contamination during a run is caught by that run's own monitor check).

Every individual run additionally required a quiet 5-second pre-run probe (the pinned `calm_preflight`) and was judged by the monitor's own samples; contaminated runs were rejected and their CPU numbers kept in the ledger.

### Host stall incident and the stalled-sample rule
**Guest freeze (whole-machine pause) during rep 2 hnsw_100k_384 hist attempt 1.** A sparse Start-Sleep poll scheduled for 590 s at about 20:05Z printed 21:27:35Z when it returned (about 82 minutes later); the campaign log had no new line since 20:04:47Z; the in-flight vlite-bench process held 502 s of CPU time at 21:27:56Z although it had started at 20:04:57Z.

- Estimated stall: about 2026-10-25T20:12:25Z (first resumed monitor sample spans 4,464.955 s and ends with the first post-resume events at 21:26:50Z); about 74.4 minutes. The system tick count advanced by 4,487,750 ms between two Time-Service events 4487.761 s apart, so both clocks include the gap.
- Affected attempt: repetition 2, `hnsw_100k_384`, hist, attempt 1 (20:04:47–21:32:49 UTC): recorded 5272.25 s instead of the usual 759-802 s; longest monitor sample 4464.955 s; verdict of the unchanged policy: rejected: mean 2.24% (diluted by the 4,465 s sample), worst sample 116.22% (limit 50%).
- The guest did not execute for about 74 minutes in the middle of the measured run; the monitor's own clock (perf_counter) and GetTickCount64 both advanced across the gap, so wall-clock elapsed time and every in-process Instant timer that spans the gap are inflated.
- The guest event logs contain no Kernel-Power, Power-Troubleshooter, Kernel-General or Terminal Services entries for the previous six hours, so the mechanism is not recorded inside the guest. The last event before and the first events after the gap are consistent with a platform-level pause or hibernate followed by a resume (dynamic-memory driver renegotiation and Kernel-Boot trace points at the resume instant). The host name prefix CPC- is consistent with a managed cloud PC, but that is an inference, not a recorded fact.
- The LastBootUpTime is unchanged (2026-10-17T07:36:10.5Z), so this was not a reboot.
- The active power plan is High performance with Hibernate after = 0 (never) on AC and DC; Hibernate and Fast Startup are the only available sleep states. An in-guest idle timer is therefore not the cause.
- At 2026-10-25T21:29:50Z quser showed the interactive session mahpate (rdp-sxs..., logon 2026-10-17 8:10 PM local) in state Active with zero idle time; the interactive user was present after the resume. Teams and Office host processes were started in that session at 21:26:59Z and 21:28:22Z.
- The unchanged CPU policy rejected this attempt only because of the post-resume burst (peak 116.22%); its mean (2.24%) was diluted to far below the 10% limit by the single 4,465 s sample. A freeze that ended quietly would not have been rejected by the policy, so the report adds a stalled-sample check to every run.
- Wall-clock span minus (preflight wait + monitor elapsed) was at most 0.05 s for all 47 records, including this one, so a wall-clock versus perf_counter comparison cannot detect such a freeze; sample length can. Across the other 46 records the longest monitor sample was 2.04 s (rep 2 hnsw_100k_384 cur attempt 2, itself rejected) and every other sample was at most 0.54 s.
- Handling: The attempt stays in runs.jsonl as rejected and is excluded from all statistics; it counts toward the 3-attempt cap of its slot like any other rejected attempt.
- Handling: No threshold was changed. The campaign continues with the unchanged frozen runner; the runner is not modified.
- Handling: The report generator (which is not part of the frozen runner) applies one additional quality rule to every run, in both directions: an accepted run is quarantined when any monitor sample lasted longer than 5 s (10 times the 0.5 s nominal). A quarantined run is listed with its reason and excluded from statistics; this makes the check stricter, never looser, than the original policy.
- Evidence file: `evidence/window/incident-vm-freeze.json`.

**Stalled-sample rule** (applied by this report generator to every run; the frozen runner is unchanged and the CPU thresholds are untouched): a run the runner accepted is quarantined and excluded from every statistic when any monitor sample lasted longer than 5 s (ten times the nominal 0.5 s). The rule is written in the incident record (`incident-vm-freeze.json`, last modified 2026-10-25T21:35:22+00:00 UTC, when 47 of the campaign's 116 recorded attempts had finished) and can only remove runs.
- Result: 0 of the runner's 58 accepted runs quarantined.
- Longest monitor sample in any run that remains accepted: 0.549 s; longest in any attempt without a stalled sample: 2.043 s; attempts containing a sample over the limit: rep 2 `hnsw_100k_384` hist attempt 1 (4465 s, rejected).

### Agent activity during the measurement window
The agent session that operated this campaign shared the host with the measured runs. While runs were in progress it polled the campaign log, read and analysed raw results and edited files, and nothing in the protocol prevented that. Its processes (PowerShell, Python, ripgrep) and the Defender scans they trigger count as background CPU, as the policy requires: runs that exceeded the unchanged limits stay rejected, and accepted runs may still contain agent-induced bursts that stayed within the limits.

- Record: `evidence/window/agent-activity.json` (sha256 `2c82f7277338b0296e4c736990a94085ab04ba4e942d80ee15fb91fa6939993e`) lists 205 tool calls from 2026-10-25T18:21:34+00:00 to 2026-10-26T01:02:07+00:00 UTC, 77 of them starting an operating-system process, and 148 model turns; by tool: powershell 56, view 55, edit 48, grep 20, read_powershell 16, create 4, get_sessions_status 2, sql 2, stop_powershell 1, session_store_sql 1. Source: the agent session's own event log (tool.execution_start/complete, external_tool.requested/completed, system.notification, assistant.turn_start and assistant.message events); only tool names, timestamps and a short note were copied.
- Method: a synchronous command counts as active from its start to its end; every other call counts at its start, at its end and, for an asynchronous shell whose completion was announced, at its exit. A monitor sample *coincides* with a call when it comes within ±5 s of such a moment. Calls that start an operating-system process (PowerShell, ripgrep and what they launch) are kept apart from in-process calls (file views, edits, waiting on a shell); a *model turn* is a model request in progress, network latency included. This measures timing only.
- Overlap with measured runs: 71 process-starting and 113 in-process calls overlapped at least one measured run (any outcome).

**Background CPU near and away from the agent's activity.** Monitor samples of at least 0.1 s from the 22 long (≥ 100 s) attempts, leaving out each run's first 20 s and the stalled attempt (rep 2 hist attempt 1) — 4.7 h in all — sorted by what the agent was doing within ±5 s of the sample (the first matching row):

| The agent was doing | all long attempts: seconds | mean background | median sample | samples over 30% | samples over 50% | accepted attempts only: seconds | mean background | samples over 30% |
| :-- | --: | --: | --: | --: | --: | --: | --: | --: |
| a process-starting call | 672 | 8.35% | 5.34% | 29 (2.2%) | 1 (0.1%) | 156 | 7.14% | 3 (1.0%) |
| only in-process calls | 500 | 8.92% | 5.25% | 42 (4.3%) | 4 (0.4%) | 273 | 8.98% | 20 (3.7%) |
| only a model turn in progress | 1,072 | 7.31% | 4.81% | 24 (1.1%) | 0 (0.0%) | 753 | 7.42% | 20 (1.3%) |
| none of these | 14,586 | 6.51% | 4.10% | 288 (1.0%) | 81 (0.3%) | 4,895 | 6.17% | 28 (0.3%) |

Rows differ in what else was going on besides the agent's tool call: the table shows how background CPU co-varied with the agent's activity, not what caused it. The means are time-weighted like the monitor's own.

**Bursts over the 50% limit.** The 18 bursts in the attempts without a stall (consecutive over-limit samples less than 10 s apart; listed in appendix D with the nearest call), by what the agent was doing within ±5 s of the burst. *Expected* is the number to expect if each burst had landed at a random place in its own attempt (a stretch of the same length slid over every position of that attempt):

| The agent was doing | bursts observed | bursts expected if unrelated |
| :-- | --: | --: |
| a process-starting call | 1 | 1.0 |
| only in-process calls | 2 | 0.4 |
| neither | 15 | 16.6 |

Process-starting calls overlapping each long (≥ 100 s) attempt, by outcome: accepted n=8, median 1.5 (min 0, max 11); rejected n=15, median 3 (min 0, max 10). Attempts that overlapped no tool call of any kind: accepted 2 of 8; rejected 2 of 15.

| rep | workload | rev | try | outcome | bg mean | bg max sample | process-starting calls | in-process calls |
| --: | :-- | :-- | --: | :-- | --: | --: | --: | --: |
| 1 | `hnsw_100k_384` | cur | 1 | rejected | 8.10% | 97.85% | 0 | 3 |
| 1 | `hnsw_100k_384` | cur | 2 | rejected | 6.46% | 52.64% | 0 | 2 |
| 1 | `hnsw_100k_384` | cur | 3 | accepted | 6.95% | 47.08% | 2 | 2 |
| 1 | `hnsw_100k_384` | hist | 1 | accepted | 5.46% | 49.22% | 1 | 3 |
| 2 | `hnsw_100k_384` | cur | 1 | rejected | 6.92% | 51.92% | 2 | 0 |
| 2 | `hnsw_100k_384` | cur | 2 | rejected | 9.33% | 167.23% | 2 | 0 |
| 2 | `hnsw_100k_384` | cur | 3 | accepted | 7.39% | 37.04% | 2 | 0 |
| 2 | `hnsw_100k_384` | hist | 1 | rejected (stalled) | 2.24% | 116.22% | 10 | 3 |
| 2 | `hnsw_100k_384` | hist | 2 | rejected | 7.20% | 93.64% | 9 | 6 |
| 2 | `hnsw_100k_384` | hist | 3 | rejected | 5.37% | 52.03% | 3 | 0 |
| 3 | `hnsw_100k_384` | cur | 1 | rejected | 6.51% | 93.71% | 3 | 0 |
| 3 | `hnsw_100k_384` | cur | 2 | rejected | 6.26% | 58.03% | 8 | 37 |
| 3 | `hnsw_100k_384` | cur | 3 | rejected | 5.97% | 93.45% | 4 | 0 |
| 3 | `hnsw_100k_384` | hist | 1 | rejected | 6.31% | 94.18% | 3 | 0 |
| 3 | `hnsw_100k_384` | hist | 2 | rejected | 6.38% | 94.08% | 8 | 1 |
| 3 | `hnsw_100k_384` | hist | 3 | accepted | 5.34% | 39.26% | 2 | 1 |
| 4 | `hnsw_100k_384` | cur | 1 | rejected | 5.70% | 51.52% | 10 | 6 |
| 4 | `hnsw_100k_384` | cur | 2 | accepted | 5.68% | 43.31% | 11 | 18 |
| 4 | `hnsw_100k_384` | hist | 1 | accepted | 8.06% | 48.51% | 1 | 34 |
| 5 | `hnsw_100k_384` | cur | 1 | accepted | 6.51% | 32.02% | 0 | 0 |
| 5 | `hnsw_100k_384` | hist | 1 | rejected | 9.63% | 85.73% | 0 | 0 |
| 5 | `hnsw_100k_384` | hist | 2 | rejected | 5.20% | 57.08% | 0 | 0 |
| 5 | `hnsw_100k_384` | hist | 3 | accepted | 6.43% | 29.45% | 0 | 0 |


Coincidence in time is not proof of cause: Defender, the VPN/zero-trust client, management agents and the guest itself also burst on their own (section 10). The tables are evidence for the reader; no run was re-labelled because of them.
The pre-registered status rule above did not cover this activity, so section 7 reports the exclusive-quiet-window requirement as **partial** rather than `met`.

## 5. Host identity

| Item | This host (captured before the measurements) | Original reference host (verbatim from the pinned baseline.md) |
| :-- | :-- | :-- |
| Computer | `[REDACTED-HOSTNAME]` (Virtual Machine, Hyper-V UEFI Release v4.1; hypervisor present: True) | – |
| OS | Microsoft Windows 11 Enterprise 10.0.26300 (26H2, UBR 9711), booted 2026-10-17T07:36:10+00:00 | Windows 11, build 26300 (`Windows-11-10.0.26300-SP0`, AMD64) |
| CPU | AMD EPYC 9V74 80-Core Processor (AMD64 Family 25 Model 17 Stepping 1), 8 cores / 16 logical, 2596 MHz, L2 8192 KB, L3 32768 KB | Intel Xeon Platinum 8370C (`Intel64 Family 6 Model 106 Stepping 6`), 16 logical CPUs |
| RAM | 68,714,033,152 bytes | 64 GiB (68,665,831,424 bytes) |
| Storage | `Q:` ReFS `DevDrive`, 1,648,126,590,976 bytes (1,369,714,028,544 free at capture) on `Msft Virtual Disk` (SAS, 2,199,023,255,552 bytes, Unspecified); Dev Drive settings not readable without administrator rights | `Q:` is a ReFS Dev Drive on a disk that Windows reports as a virtual SAS disk (`Msft Virtual Disk`); the physical backing is unknown |
| Power plan / time zone | High performance; (UTC-08:00) Pacific Time (US & Canada) | – |
| Security software | Defender real-time protection True; exclusions not viewable (not administrator); sensor and management agents run continuously (see window evidence) | The reference host is described as never idle either (security and management agents) |
| Interactive sessions | `mahpate rdp-sxs26073... 2 Active 6 10/17/2026 8:10 PM` (`query user`); WSL `Ubuntu-24.04` was running and was left untouched | – |


Identity checks against the reference host (the pinned `graph_reference.main()` gate, evaluated for the record only; its `main()` was not run):

| Check | Result |
| :-- | :-- |
| cpu_is_xeon_8370c | **NO** |
| logical_cpus_16 | yes |
| ram_bytes_equal_reference | **NO** |
| os_version_10.0.26300 | yes |
| volume_Q_ReFS | yes |
| disk_msft_virtual_sas | yes |


The CPU model and the RAM size differ from the reference host, so this is a **different machine**; results here are not comparable with the original baseline for REG-010 purposes.

## 6. Pinned sources, toolchain, binaries and runner
Revisions pinned 2026-10-25T17:37:09.436+00:00; `git ls-remote origin main` agreed with the fetched `origin/main`; the historical commit is an ancestor of the current one (43 commits between). An earlier fetch of `origin/main` (`41cb396b0bc5a4ee6383924ddff4fb14ab1ff7ba`) was superseded before anything was built or measured.

| Revision | Commit | Tree | Subject | Committer date |
| :-- | :-- | :-- | :-- | :-- |
| historical | `81ff901c54d674d1ad519dd4fa722e937db46c4b` | `da11989ef426f7f41835616300670e7b60928b9a` | Restore feature comparisons and comparative benchmarks | 2026-10-23T15:27:09-07:00 |
| current origin/main | `c024dcea72f1479e2114254e6d6388dcb5657315` | `96a35ceb7fa211dc78c6839ceb2a20fe49940b8a` | FOLLOWUP-001: add source-only opt-in graph CLI | 2026-10-25T10:28:36-07:00 |



| Source archive | Files | Tar SHA-256 | Source roll-up SHA-256 | Manifest SHA-256 |
| :-- | :-- | :-- | :-- | :-- |
| historical | 344 | `0d37e63881eb3f56d2279267775c465b77454a0c9ad62f2e5ee4122da0f3b48f` | `380d0472d8edac3860eeb754d809832e8392759149e5e0600ef85fbf371f65ab` | `75457908583fc501…` |
| current origin/main | 556 | `72a394b41374900373c84b11229e263ee31a48bde5b2404a40a747fbcd26c13c` | `1f6de2a7e78a135a45ae27e2799ad08479a26da03a39b4942d09908d55688c0d` | `97af23c6ef3e8220…` |


Archives were produced with `git -c core.autocrlf=false -c core.eol=lf archive` (this host's git has `core.autocrlf=true`, which would otherwise write CRLF) and every file was verified against its git blob; the repository's own `graph_compat_check.extract` uses a plain `git archive`. Both revisions are treated identically.


| Revision | Binary | Bytes | SHA-256 |
| :-- | :-- | :-- | :-- |
| historical | vlite-bench | 1,146,368 | `2ed6b33838cabd92465547937b6468c8868b3a215d07ce01c55b94f4347bf7ea` |
| historical | vlite-ffi | 2,830,336 | `6dfeb7c75731c71f7ccdeb09c1045ec78e6a1efdf0d3b26f2d2a09f2d7d1c08d` |
| current origin/main | vlite-bench | 1,131,008 | `aad2c49f1893ebbd53873ccebfca333f19b848158a1fcdf32b49cd553d10c2a5` |
| current origin/main | vlite-ffi | 2,876,928 | `5db93ad44a7f53d99f7bfb44ef626b50ef378cdcda162f0e2e73d8b9840f42a1` |


`src/bin/vlite-bench.rs` is byte-identical in both revisions (sha256 `1df0ed1fb42c5325…`), so both revisions are measured by the same harness; only the library differs. The bench reports engine versions `0.3.0` (historical), `0.4.1` (current origin/main).

Toolchain: rustc 1.99.1 (58cf8f9c3 2026-10-13); cargo 1.99.1 (5f94df478 2026-08-27); target `x86_64-pc-windows-msvc`; Python 3.15.0; linker `C:\Program Files\Microsoft Visual Studio\18\Enterprise\VC\Tools\MSVC\14.51.36231\bin\HostX64\x64\link.exe` (sha256 `a8c42e25c91ca2453e1110540ac08f0fc12d7539454a11b9af8baeecb469837d`). Builds ran offline and locked after one online `cargo fetch` per revision; `CARGO_INCREMENTAL=0`; inherited Rust/Cargo environment variables were dropped.

Runner and pinned-script hashes (the measurement modules were recorded before the campaign and re-checked when this report was generated):

| File | SHA-256 | unchanged since recorded |
| :-- | :-- | :-- |
| runner/do005_campaign.py | `21af22cf2fdf72691ff8acf5d30562d7ad17063e196f860383f45fcfbfc15770` | yes |
| runner/do005_common.py | `c2b41a1e2c5f5404fbeba142b9b49d9c655a88865a197b11ad36896b8d2fe2c3` | yes |
| runner/do005_prepare.py | `e4ffff047e2714d74585d0c197bba1ae8e8c88d9943e09be6fb5cdeca8be2d47` | yes |
| pinned scripts/graph_reference.py | `c520e0b759c16d6ce741ee91a0ff71120acb011c970b534ec49e62462d83ee5e` | yes |
| pinned scripts/graph_baseline.py | `5fd4944ef31063b82dfe81a8f2a4ae1476a4ec79c8593caff5925de04e7c12c4` | yes |
| pinned scripts/graph_compat_check.py | `e59ef77debc77e00f3033757e0def3eceed4dd952c25161b7a52a79f27efbe55` | yes |
| pinned tests/measure_benchmark.py | `e49afae2cb22d1181f0fa5801f8e4937cc25c9e8e968ff88220c37724c2f0541` | yes |
| python.exe | `727b92dc55e1ac0f17a48cbd20557427fdb8d9f50731de0355c4fd4c08d2f473` | – |
| runner/do005_report.py (report generator) | `81c7cf259ee39e7b74c9bb74e389221693e277de9c477a64550a108f92be173a` | post-processing only; hash taken when this report was generated |


## 7. Protocol and requirement checks

| Requirement | Status | Detail | Evidence |
| :-- | :-- | :-- | :-- |
| Both revisions pinned to full commit SHAs | met | hist `81ff901c54d674d1ad519dd4fa722e937db46c4b`, cur `c024dcea72f1479e2114254e6d6388dcb5657315` | pins.json |
| Separate clean `git archive` trees | met | every extracted file verified against its git blob SHA-1; no `.git` directory in either tree; re-hashed at campaign start (ok) and end (ok) | archives.json, source-manifest-*.json, raw/<tag>/sources-verified-*.json |
| Identical toolchain; release, default-feature builds | met | rustc/cargo 1.99.1-x86_64-pc-windows-msvc; every build ran `cargo build --locked --offline --release` (flags verified in builds.json); the `graph` feature and the graph crate were not compiled in either build | toolchain.json, builds.json, logs/cargo-* |
| Identical workload arguments for both revisions | met | each workload used one argument list for every recorded attempt of both revisions | raw/<tag>/runs.jsonl, campaign-config.json |
| Identical storage location | met | VLITE_BENCH_DIR, TEMP and TMP = Q:\vlite-campaign\do005-sephost-20261025T172902Z\work\db (Q:, ReFS) | raw/<tag>/runs.jsonl |
| 5 repetitions of every smoke/reference workload | NOT MET | 58 of 60 (workload x revision x repetition) slots have an accepted run | raw/<tag>/runs.jsonl |
| Dependencies restored before the quiet window; locked/offline builds | met | `cargo fetch` and both builds finished before the first window survey | builds.json, evidence/window/ |
| CPU policy unchanged (mean <= 10 %, any >= 0.1 s sample <= 50 %, scanner/System counted as background) | met | pinned monitor sha256 `c520e0b759c16d6c…`; constants {"MAX_BACKGROUND_MEAN": 0.1, "MAX_BACKGROUND_SAMPLE": 0.5, "SAMPLE_SECONDS": 0.5, "IDLE_SECONDS": 5, "PROBE_ATTEMPTS": 12} | selftest.json |
| The Windows CPU monitor was used for every measured run | met | every benchmark ran through the pinned `graph_reference.run_process`; `graph_baseline.py measure` imports the CPU monitor: False; `measure_benchmark.py` samples CPU: False (it records peak working set only) | selftest.json, runs.jsonl |
| Contaminated runs rejected, thresholds not relaxed | met | 58 rejected, 0 refused, 0 quarantined by the report's stalled-sample rule (a monitor sample longer than 5 s; the thresholds themselves were not touched); boundary tests and live violation tests passed: True | selftest.json, evidence/window/incident-vm-freeze.json |
| Exclusive quiet window verified | partial | required: pre-window survey met the unchanged policy: yes; no build/benchmark processes at start: yes; no other busy app sessions before the start: yes — also reported: post survey met the policy: yes; no build/benchmark processes at end: yes; no other busy app sessions after the end: yes. Every run additionally needed its own quiet pre-run probe and monitor-measured CPU within policy. Exclusivity of a shared host can be checked, not proven (see limitations). The pre-registered rule did not cover the agent session's own activity: 71 process-starting and 113 in-process tool calls overlapped measured runs (section 4), so exclusivity is only partial. | evidence/window/, raw/<tag>/census-*.json |
| Host identity, OS, CPU, RAM, storage, toolchain, source/binary/runner hashes recorded | met | see the evidence directory and the sections below | host.json, toolchain.json, builds.json, archives.json, runner-hashes.json |
| Baseline documents not modified, no gate approved, nothing committed or pushed | met | `git status` clean in the clone (True) and the session worktree (True); the runner only reads sources through `git archive`, `fetch`, `ls-remote` and `rev-parse` | git state section |


Git state at report time — clone `Q:\vlite`: HEAD `41cb396b0bc5`, clean=True; session worktree: HEAD `41cb396b0bc5` on `mahpate-bookish-robot`, clean=True. Neither working tree was used for measuring or building; the clone served only as the source for `git archive`.

**`origin/main` advanced after the pin.** The measured revision stays the pinned `c024dcea72f1479e2114254e6d6388dcb5657315`, as the protocol requires; at report time `origin/main` was `adde71b91c9e865d4bc08628717cca7d72f7baa0` (9 commits after the pin, 86 files changed, +8,300/−4,709 lines). Nothing at the newer head was built or measured.

- `adde71b91c9e865d4bc08628717cca7d72f7baa0 2026-10-25T18:06:39-07:00 Recapture connected dashboard tour with the sign-in dashboard assets`
- `899ffbccacbc02f2869645125fd69086119c9d2f 2026-10-25T17:56:17-07:00 Trim the README, website and guides; document sign-in`
- `8964b035f1de7a2f829aa3ca62526196168c4b7e 2026-10-25T17:56:17-07:00 Add dashboard sign-in, password change and user management`
- `afdc45abdcd98a10cf4cffbc3febc73ce8f925e5 2026-10-25T17:56:16-07:00 Add dashboard user accounts with password sessions`
- `280bd620ec141bfeaf158356ab6f5995a2af335d 2026-10-25T14:56:39-07:00 Speed up Python float32 ingest and add typed ready-only ANN search`
- `0011e012dd960444f54b94a1270a857b0c1400cb 2026-10-25T11:13:04-07:00 Recapture connected dashboard tour for identity and tenant lifecycle`
- `6cf5d122ef2529395b82e0ca77e95b3ed687c7dc 2026-10-25T11:04:42-07:00 Show the full super-admin identity label on desktop`
- `e80ffb36db759c0a11ab48fd8c9966dc726b7fd0 2026-10-25T10:59:36-07:00 Wrap the identity badge on narrow screens`
- `572b5b361776548d40ff2e28e144a7e38f094f44 2026-10-25T10:41:12-07:00 Show signed-in identity and add tenant lifecycle administration`

Files changed after the pin, classified by path and `cfg` gating at the newer head (default features there: `[]`; `pub mod server;` behind `#[cfg(feature = "server")]`: True; `src/bin/vlite-bench*` changed: False):

| Category | Files | +lines | −lines |
| :-- | --: | --: | --: |
| documentation and site assets | 52 | 2,639 | 4,545 |
| tests and benches (not built by `cargo build --bin vlite-bench`) | 11 | 2,375 | 24 |
| other (needs review) | 10 | 183 | 14 |
| assets embedded only by server code | 7 | 700 | 87 |
| server code (`#[cfg(feature = "server")]`) | 5 | 2,396 | 39 |
| build input (Cargo.toml, Cargo.lock, build.rs, toolchain) | 1 | 7 | 0 |


**REVIEW NEEDED: the changes after the pin cannot be shown to be outside the default-feature `vlite-bench` build (default features [], server module gated: True, paths needing review: 11). The newer head was not built or measured.**

Paths needing review:
- `.github/workflows/pages.yml`
- `Cargo.toml`
- `benchmarks/compare_engines.py`
- `benchmarks/compare_zvec.py`
- `bindings/c/src/lib.rs`
- `bindings/c/src/typed.rs`
- `bindings/c/vlite.h`
- `bindings/python/test_typed_vectors.py`
- `bindings/python/vlite_embedded/__init__.py`
- `compose.yaml`
- `src/bin/vlite-server.rs`
If the newer head is wanted as the comparison revision, the campaign must be re-pinned and rebuilt; this report does not do that.

## 8. Determinism checks
Deterministic outputs (checksums, file sizes, recall, returned counts) over every attempt that produced parseable output (accepted and rejected alike; CPU contamination cannot change them).

| Workload | runs hist/cur | stable within each revision | keys identical across revisions | keys that differ across revisions |
| :-- | --: | :-: | --: | :-- |
| `exact_small` | 11/10 | yes | 13/13 | none |
| `filtered_exact` | 11/10 | yes | 13/13 | none |
| `hnsw_small` | 13/16 | yes | 20/20 | none |
| `exact_100k_384` | 6/5 | yes | 13/13 | none |
| `filtered_100k_384` | 6/5 | yes | 13/13 | none |
| `hnsw_100k_384` | 11/12 | yes | 20/20 | none |


## 9. Informational only — this host's historical-revision medians vs the original baseline
The pinned `baseline.md` measured the **same commit `81ff901`** on the reference host (5 runs, medians, no CPU-policy check). Its own text says results are comparable only with reruns on that machine and volume, so this table is context for the host difference, **not** a REG-010 comparison and not a replacement for the original numbers.

| Workload | Metric | Original host (verbatim cell) | This host, hist, median | This / original |
| :-- | :-- | :-- | --: | --: |
| `exact_small` | Ingest (vectors/s) | 42,446 | 53,108 (n=5) | 1.25× |
| `exact_small` | Query p50 (ms) (exact) | 0.0021 | 0.0018 (n=5) | 0.86× |
| `exact_small` | Query p99 (ms) (exact) | 0.0026 | 0.0025 (n=5) | 0.96× |
| `exact_small` | Reopen (ms) | 0.365 | 0.1870 (n=5) | 0.51× |
| `exact_small` | Peak RSS (MiB) | 5.0 | 4.625 (n=5) | 0.93× |
| `exact_small` | Recall (exact) | 1.0 | 1 (n=5) | equal |
| `filtered_exact` | Ingest (vectors/s) | 110,117 | 143,135 (n=5) | 1.30× |
| `filtered_exact` | Query p50 (ms) (exact) | 0.0188 | 0.0196 (n=5) | 1.04× |
| `filtered_exact` | Query p99 (ms) (exact) | 0.0242 | 0.0213 (n=5) | 0.88× |
| `filtered_exact` | Reopen (ms) | 1.484 | 1.415 (n=5) | 0.95× |
| `filtered_exact` | Peak RSS (MiB) | 5.6 | 5.195 (n=5) | 0.93× |
| `filtered_exact` | Recall (exact) | 1.0 | 1 (n=5) | equal |
| `hnsw_small` | Ingest (vectors/s) | 104,447 | 156,088 (n=5) | 1.49× |
| `hnsw_small` | Query p50 (ms) (exact) | 0.0088 (exact) / 0.0501 (HNSW) | 0.0054 (n=5) | 0.61× |
| `hnsw_small` | Query p50 (ms) (HNSW) | 0.0088 (exact) / 0.0501 (HNSW) | 0.0402 (n=5) | 0.80× |
| `hnsw_small` | Query p99 (ms) (exact) | 0.0106 (exact) / 0.0690 (HNSW) | 0.0058 (n=5) | 0.55× |
| `hnsw_small` | Query p99 (ms) (HNSW) | 0.0106 (exact) / 0.0690 (HNSW) | 0.0483 (n=5) | 0.70× |
| `hnsw_small` | Reopen (ms) | 0.547 | 0.2824 (n=5) | 0.52× |
| `hnsw_small` | Peak RSS (MiB) | 5.1 | 4.762 (n=5) | 0.93× |
| `hnsw_small` | Recall (exact) | 1.0 / 1.0 | 1 (n=5) | equal |
| `hnsw_small` | Recall (HNSW) | 1.0 / 1.0 | 1 (n=5) | equal |
| `hnsw_small` | HNSW index build (ms) | 19.465 | 15.778 (n=5) | 0.81× |
| `exact_100k_384` | Ingest (vectors/s) | 62,489 | 66,710 (n=5) | 1.07× |
| `exact_100k_384` | Query p50 (ms) (exact) | 9.968 | 8.936 (n=5) | 0.90× |
| `exact_100k_384` | Query p99 (ms) (exact) | 12.080 | 9.930 (n=5) | 0.82× |
| `exact_100k_384` | Reopen (ms) | 1,131.702 | 1,172 (n=5) | 1.04× |
| `exact_100k_384` | Peak RSS (MiB) | 329.0 | 328.2 (n=5) | 1.00× |
| `exact_100k_384` | Recall (exact) | 1.0 | 1 (n=5) | equal |
| `filtered_100k_384` | Ingest (vectors/s) | 56,274 | 59,904 (n=5) | 1.06× |
| `filtered_100k_384` | Query p50 (ms) (exact) | 4.762 | 5.532 (n=5) | 1.16× |
| `filtered_100k_384` | Query p99 (ms) (exact) | 8.039 | 5.850 (n=5) | 0.73× |
| `filtered_100k_384` | Reopen (ms) | 1,179.414 | 1,221 (n=5) | 1.03× |
| `filtered_100k_384` | Peak RSS (MiB) | 384.7 | 384.8 (n=5) | 1.00× |
| `filtered_100k_384` | Recall (exact) | 1.0 | 1 (n=5) | equal |
| `hnsw_100k_384` | Ingest (vectors/s) | 60,903 | 66,210 (n=4) | 1.09× |
| `hnsw_100k_384` | Query p50 (ms) (exact) | 8.777 (exact) / **3.072 (HNSW)** | 8.991 (n=4) | 1.02× |
| `hnsw_100k_384` | Query p50 (ms) (HNSW) | 8.777 (exact) / **3.072 (HNSW)** | 3.056 (n=4) | 0.99× |
| `hnsw_100k_384` | Query p99 (ms) (exact) | 11.315 (exact) / **3.630 (HNSW)** | 9.962 (n=4) | 0.88× |
| `hnsw_100k_384` | Query p99 (ms) (HNSW) | 11.315 (exact) / **3.630 (HNSW)** | 3.697 (n=4) | 1.02× |
| `hnsw_100k_384` | Reopen (ms) | 1,097.084 | 1,167 (n=4) | 1.06× |
| `hnsw_100k_384` | Peak RSS (MiB) | 355.9 | 355.7 (n=4) | 1.00× |
| `hnsw_100k_384` | Recall (exact) | 1.0 (exact) / **0.26 (HNSW)** | 1 (n=4) | equal |
| `hnsw_100k_384` | Recall (HNSW) | 1.0 (exact) / **0.26 (HNSW)** | 0.26 (n=4) | equal |
| `hnsw_100k_384` | HNSW index build (ms) | 930,819.669 | 776,356 (n=4) | 0.83× |


## 10. Limitations
- **Host noise floor.** A 599 s exploratory survey of the otherwise unused host had a mean of 12.03% and a worst sample of 63.25%; only 52 of 119 five-second probes were individually quiet; top contributors SenseCE.exe 4.43%, svchost.exe 1.21%, GlobalSecureAccessClient.exe 1.11%, WmiPrvSE.exe 0.63%. The pinned monitor documents the reference host as about 5 % busy with bursts near 45 %; the policy is calibrated to that floor. The long `hnsw_100k_384` runs (about 13 minutes each) were accepted in 8 of 23 measured attempts; of the 15 rejected ones, 15 failed only the 50 % per-sample limit, 0 only the 10 % mean limit and 0 both (section 3, appendix D). Nothing was relaxed; slots that could not be measured are reported as missing.
- **Host stall.** The guest did not run for about 74 minutes once during the campaign (section 4); that attempt was rejected, and this report quarantines any run the runner accepted if it contains a monitor sample over 5 s (0 quarantined; longest sample in a remaining accepted run 0.55 s). The mechanism is not recorded inside the guest, so a platform-level pause is an inference, and stalls shorter than the check cannot be ruled out beyond what the monitor's sample lengths show.
- **Different machine.** The CPU (AMD EPYC 9V74 vs Intel Xeon Platinum 8370C) and the exact RAM size differ from the reference host; only the logical CPU count, OS build and storage type match.
- **Short runs and the monitor.** The smoke workloads finish in tens of milliseconds, far below the monitor's 0.5 s sampling interval and its 0.1 s floor for the peak rule, so for them the peak check is vacuous and acceptance rests on a mean over a handful of scheduler ticks (system times advance in clock-tick quanta of roughly 15.6 ms). Across this campaign's 71 smoke attempts the monitor's background mean ranged from 2.73% to 53.06%. Their CPU check is therefore weak, and retrying until a run passes is a selection effect; appendix B shows how far accepted-only statistics differ from all-attempt statistics.
- **Shared host and agent activity.** The agent harness that ran the campaign and the user's desktop session run on the same machine. Their CPU counts as background, as the policy requires, and the agent session was **not** idle during measurement: it polled the campaign log and analysed results while runs were in progress (section 4, "Agent activity during the measurement window"). In the long runs, background CPU averaged 8.35% in samples within ±5 s of a process-starting call and 6.51% in samples near no call or model turn, and 1 of the 18 bursts over the 50 % limit outside the stalled attempt coincided with such a call (1.0 expected if unrelated); appendix D lists every burst. That timing evidence neither excludes nor establishes a contribution to any individual rejection or to bursts inside accepted runs. Exclusivity of a shared host can be checked, not proven. In both exploratory surveys a sensor (`SenseCE.exe`) used about 4.4 % of all CPUs on its own; it, the VPN/zero-trust client, management agents and Defender could not be paused (not administrator, and they belong to the organisation).
- **n ≤ 5.** Medians and ranges only; no confidence intervals or significance claims. Order within each pair alternates to balance warm-cache effects but cannot remove them.
- **Storage.** Both revisions used the same Q: ReFS Dev Drive directory; the physical backing of the virtual disk is unknown (as on the reference host) and Dev Drive/Defender-exclusion settings could not be read without administrator rights.
- **Scope.** Default-feature (graph off) release builds of `vlite-bench` only. Graph-on builds, `vlite-graph-cli` and the REG-013 build-boundary check were not part of this campaign.
- **Archive line endings.** Both archives were LF-normalised identically and verified against their git blobs; the repository's own helper would produce CRLF on this host. Whether line endings could change the compiled binaries was not tested, and a byte-for-byte reproduction of the original host's build inputs is not claimed.
- **Moving `origin/main`.** An earlier fetch (`41cb396b0bc5`) was already superseded before anything was built; at report time the remote head was `adde71b91c9e`, 9 commits after the pinned revision (section 7). The pinned commit is the only revision measured.

## Artifacts
- Raw outputs: `raw/main/runs.jsonl` holds one JSON record per attempt — accepted, rejected, refused or not attempted — with the child's complete stdout and stderr, the monitor's per-sample CPU, the pre-run probes and process censuses; `campaign.log`, `campaign-config.json`, the start/end process censuses and the start/end source re-verification sit beside it.
- Evidence: `evidence/` (pins, archives, toolchain, builds, host, reference-identity checks, runner and pinned-script hashes, self-test, window surveys, the host stall incident record, build logs).
- Integrity: `ARTIFACTS.sha256` lists the SHA-256 of every evidence, raw, runner and report file; its roll-up is in `ARTIFACTS.rollup.txt`. The roll-up cannot be printed inside this report, which is one of the hashed files.
- Machine-readable twin of this report: `comparison-report.json`.
- Other data under `raw/` (runner self-tests and dry runs, kept for transparency and excluded from every statistic here): `dryrun` (11 records), `dryrun2` (12 records), `selftest` (0 records). The dry runs' window surveys are in `evidence/window/dryrun/`.

## Appendix A — attempt ledger (chronological, every attempt)

| # | UTC | rep | workload | rev | try | outcome | wall s | bg mean | bg max sample | note |
| --: | :-- | --: | :-- | :-- | --: | :-- | --: | --: | --: | :-- |
| 1 | 18:30:03 | 1 | `exact_small` | cur | 1 | accepted | 0.034 | 5.80% | n/a |  |
| 2 | 18:30:08 | 1 | `exact_small` | hist | 1 | accepted | 0.035 | 2.90% | n/a |  |
| 3 | 18:30:13 | 1 | `filtered_exact` | hist | 1 | accepted | 0.041 | 7.63% | n/a |  |
| 4 | 18:30:18 | 1 | `filtered_exact` | cur | 1 | rejected | 0.042 | 21.99% | n/a | mean > 10% |
| 5 | 18:30:28 | 1 | `filtered_exact` | cur | 2 | rejected | 0.036 | 10.85% | n/a | mean > 10% |
| 6 | 18:30:38 | 1 | `filtered_exact` | cur | 3 | accepted | 0.036 | 5.51% | n/a |  |
| 7 | 18:30:43 | 1 | `hnsw_small` | cur | 1 | rejected | 0.051 | 13.38% | n/a | mean > 10% |
| 8 | 18:30:49 | 1 | `hnsw_small` | cur | 2 | rejected | 0.043 | 13.70% | n/a | mean > 10% |
| 9 | 18:30:54 | 1 | `hnsw_small` | cur | 3 | rejected | 0.048 | 12.29% | n/a | mean > 10% |
| 10 | 18:30:59 | 1 | `hnsw_small` | cur | 4 | rejected | 0.044 | 11.22% | n/a | mean > 10% |
| 11 | 18:31:04 | 1 | `hnsw_small` | cur | 5 | accepted | 0.046 | 8.45% | n/a |  |
| 12 | 18:31:09 | 1 | `hnsw_small` | hist | 1 | accepted | 0.050 | 3.96% | n/a |  |
| 13 | 18:31:19 | 1 | `exact_100k_384` | hist | 1 | accepted | 3.944 | 4.66% | 8.31% |  |
| 14 | 18:31:28 | 1 | `exact_100k_384` | cur | 1 | accepted | 1.943 | 4.32% | 6.06% |  |
| 15 | 18:31:40 | 1 | `filtered_100k_384` | cur | 1 | accepted | 2.198 | 3.56% | 5.86% |  |
| 16 | 18:31:47 | 1 | `filtered_100k_384` | hist | 1 | accepted | 3.951 | 4.92% | 9.36% |  |
| 17 | 18:31:56 | 1 | `hnsw_100k_384` | hist | 1 | accepted | 759.170 | 5.46% | 49.22% |  |
| 18 | 18:44:40 | 1 | `hnsw_100k_384` | cur | 1 | rejected | 771.859 | 8.10% | 97.85% | sample > 50% |
| 19 | 18:57:37 | 1 | `hnsw_100k_384` | cur | 2 | rejected | 763.640 | 6.46% | 52.64% | sample > 50% |
| 20 | 19:10:26 | 1 | `hnsw_100k_384` | cur | 3 | accepted | 764.442 | 6.95% | 47.08% |  |
| 21 | 19:23:20 | 2 | `exact_small` | hist | 1 | rejected | 0.031 | 22.16% | n/a | mean > 10% |
| 22 | 19:23:26 | 2 | `exact_small` | hist | 2 | rejected | 0.028 | 24.50% | n/a | mean > 10% |
| 23 | 19:23:31 | 2 | `exact_small` | hist | 3 | accepted | 0.027 | 3.68% | n/a |  |
| 24 | 19:23:36 | 2 | `exact_small` | cur | 1 | rejected | 0.029 | 13.51% | n/a | mean > 10% |
| 25 | 19:23:41 | 2 | `exact_small` | cur | 2 | rejected | 0.027 | 10.83% | n/a | mean > 10% |
| 26 | 19:23:46 | 2 | `exact_small` | cur | 3 | accepted | 0.028 | 7.10% | n/a |  |
| 27 | 19:23:51 | 2 | `filtered_exact` | cur | 1 | accepted | 0.036 | 2.77% | n/a |  |
| 28 | 19:23:56 | 2 | `filtered_exact` | hist | 1 | rejected | 0.041 | 28.99% | n/a | mean > 10% |
| 29 | 19:24:01 | 2 | `filtered_exact` | hist | 2 | rejected | 0.037 | 13.19% | n/a | mean > 10% |
| 30 | 19:24:06 | 2 | `filtered_exact` | hist | 3 | rejected | 0.038 | 20.68% | n/a | mean > 10% |
| 31 | 19:24:11 | 2 | `filtered_exact` | hist | 4 | accepted | 0.036 | 8.27% | n/a |  |
| 32 | 19:24:16 | 2 | `hnsw_small` | hist | 1 | rejected | 0.048 | 19.27% | n/a | mean > 10% |
| 33 | 19:24:21 | 2 | `hnsw_small` | hist | 2 | accepted | 0.045 | 6.53% | n/a |  |
| 34 | 19:24:27 | 2 | `hnsw_small` | cur | 1 | rejected | 0.049 | 44.16% | n/a | mean > 10% |
| 35 | 19:24:32 | 2 | `hnsw_small` | cur | 2 | rejected | 0.044 | 13.46% | n/a | mean > 10% |
| 36 | 19:24:37 | 2 | `hnsw_small` | cur | 3 | rejected | 0.044 | 17.89% | n/a | mean > 10% |
| 37 | 19:24:42 | 2 | `hnsw_small` | cur | 4 | accepted | 0.045 | 6.61% | n/a |  |
| 38 | 19:24:47 | 2 | `exact_100k_384` | cur | 1 | accepted | 1.975 | 4.65% | 5.92% |  |
| 39 | 19:24:54 | 2 | `exact_100k_384` | hist | 1 | rejected | 3.834 | 15.39% | 34.40% | mean > 10% |
| 40 | 19:25:03 | 2 | `exact_100k_384` | hist | 2 | accepted | 3.752 | 5.96% | 13.51% |  |
| 41 | 19:25:22 | 2 | `filtered_100k_384` | hist | 1 | rejected | 4.035 | 14.16% | 41.15% | mean > 10% |
| 42 | 19:25:31 | 2 | `filtered_100k_384` | hist | 2 | accepted | 3.996 | 9.90% | 16.31% |  |
| 43 | 19:25:40 | 2 | `filtered_100k_384` | cur | 1 | accepted | 2.230 | 9.16% | 19.75% |  |
| 44 | 19:25:47 | 2 | `hnsw_100k_384` | cur | 1 | rejected | 762.920 | 6.92% | 51.92% | sample > 50% |
| 45 | 19:38:35 | 2 | `hnsw_100k_384` | cur | 2 | rejected | 801.736 | 9.33% | 167.23% | sample > 50% |
| 46 | 19:52:02 | 2 | `hnsw_100k_384` | cur | 3 | accepted | 759.939 | 7.39% | 37.04% |  |
| 47 | 20:04:47 | 2 | `hnsw_100k_384` | hist | 1 | rejected | 5272.254 | 2.24% | 116.22% | sample > 50% |
| 48 | 21:32:49 | 2 | `hnsw_100k_384` | hist | 2 | rejected | 795.554 | 7.20% | 93.64% | sample > 50% |
| 49 | 21:46:15 | 2 | `hnsw_100k_384` | hist | 3 | rejected | 781.467 | 5.37% | 52.03% | sample > 50% |
| 50 | 21:59:21 | 3 | `exact_small` | cur | 1 | accepted | 0.183 | 9.11% | 9.11% |  |
| 51 | 21:59:27 | 3 | `exact_small` | hist | 1 | accepted | 0.030 | 6.62% | n/a |  |
| 52 | 21:59:32 | 3 | `filtered_exact` | hist | 1 | accepted | 0.036 | 2.73% | n/a |  |
| 53 | 21:59:37 | 3 | `filtered_exact` | cur | 1 | accepted | 0.025 | 3.90% | n/a |  |
| 54 | 21:59:42 | 3 | `hnsw_small` | cur | 1 | rejected | 0.032 | 21.46% | n/a | mean > 10% |
| 55 | 21:59:47 | 3 | `hnsw_small` | cur | 2 | rejected | 0.032 | 12.41% | n/a | mean > 10% |
| 56 | 21:59:52 | 3 | `hnsw_small` | cur | 3 | accepted | 0.032 | 9.32% | n/a |  |
| 57 | 21:59:57 | 3 | `hnsw_small` | hist | 1 | rejected | 0.046 | 14.93% | n/a | mean > 10% |
| 58 | 22:00:02 | 3 | `hnsw_small` | hist | 2 | rejected | 0.046 | 10.66% | n/a | mean > 10% |
| 59 | 22:00:07 | 3 | `hnsw_small` | hist | 3 | rejected | 0.045 | 10.97% | n/a | mean > 10% |
| 60 | 22:00:12 | 3 | `hnsw_small` | hist | 4 | rejected | 0.045 | 13.14% | n/a | mean > 10% |
| 61 | 22:00:18 | 3 | `hnsw_small` | hist | 5 | accepted | 0.045 | 8.68% | n/a |  |
| 62 | 22:00:23 | 3 | `exact_100k_384` | hist | 1 | accepted | 3.788 | 4.67% | 11.18% |  |
| 63 | 22:00:32 | 3 | `exact_100k_384` | cur | 1 | accepted | 2.014 | 2.42% | 3.02% |  |
| 64 | 22:00:39 | 3 | `filtered_100k_384` | cur | 1 | accepted | 2.223 | 1.54% | 2.34% |  |
| 65 | 22:00:46 | 3 | `filtered_100k_384` | hist | 1 | accepted | 4.017 | 5.37% | 16.08% |  |
| 66 | 22:00:55 | 3 | `hnsw_100k_384` | hist | 1 | rejected | 788.122 | 6.31% | 94.18% | sample > 50% |
| 67 | 22:14:08 | 3 | `hnsw_100k_384` | hist | 2 | rejected | 784.747 | 6.38% | 94.08% | sample > 50% |
| 68 | 22:27:18 | 3 | `hnsw_100k_384` | hist | 3 | accepted | 782.911 | 5.34% | 39.26% |  |
| 69 | 22:40:26 | 3 | `hnsw_100k_384` | cur | 1 | rejected | 794.995 | 6.51% | 93.71% | sample > 50% |
| 70 | 22:53:46 | 3 | `hnsw_100k_384` | cur | 2 | rejected | 799.375 | 6.26% | 58.03% | sample > 50% |
| 71 | 23:07:10 | 3 | `hnsw_100k_384` | cur | 3 | rejected | 806.163 | 5.97% | 93.45% | sample > 50% |
| 72 | 23:20:42 | 4 | `exact_small` | hist | 1 | rejected | 0.034 | 14.41% | n/a | mean > 10% |
| 73 | 23:20:47 | 4 | `exact_small` | hist | 2 | accepted | 0.027 | 7.38% | n/a |  |
| 74 | 23:20:52 | 4 | `exact_small` | cur | 1 | rejected | 0.022 | 17.69% | n/a | mean > 10% |
| 75 | 23:20:57 | 4 | `exact_small` | cur | 2 | rejected | 0.017 | 35.37% | n/a | mean > 10% |
| 76 | 23:21:07 | 4 | `exact_small` | cur | 3 | rejected | 0.016 | 18.50% | n/a | mean > 10% |
| 77 | 23:21:12 | 4 | `exact_small` | cur | 4 | accepted | 0.016 | 6.16% | n/a |  |
| 78 | 23:21:17 | 4 | `filtered_exact` | cur | 1 | rejected | 0.028 | 14.04% | n/a | mean > 10% |
| 79 | 23:21:22 | 4 | `filtered_exact` | cur | 2 | accepted | 0.024 | 8.21% | n/a |  |
| 80 | 23:21:27 | 4 | `filtered_exact` | hist | 1 | accepted | 0.040 | 4.96% | n/a |  |
| 81 | 23:21:33 | 4 | `hnsw_small` | hist | 1 | rejected | 0.049 | 10.04% | n/a | mean > 10% |
| 82 | 23:21:38 | 4 | `hnsw_small` | hist | 2 | accepted | 0.046 | 6.46% | n/a |  |
| 83 | 23:21:43 | 4 | `hnsw_small` | cur | 1 | accepted | 0.036 | 8.15% | n/a |  |
| 84 | 23:21:48 | 4 | `exact_100k_384` | cur | 1 | accepted | 2.030 | 7.02% | 9.55% |  |
| 85 | 23:21:55 | 4 | `exact_100k_384` | hist | 1 | accepted | 3.795 | 7.10% | 15.61% |  |
| 86 | 23:22:04 | 4 | `filtered_100k_384` | hist | 1 | accepted | 4.036 | 5.59% | 15.39% |  |
| 87 | 23:22:13 | 4 | `filtered_100k_384` | cur | 1 | accepted | 2.219 | 2.77% | 4.10% |  |
| 88 | 23:22:20 | 4 | `hnsw_100k_384` | cur | 1 | rejected | 792.879 | 5.70% | 51.52% | sample > 50% |
| 89 | 23:35:38 | 4 | `hnsw_100k_384` | cur | 2 | accepted | 797.696 | 5.68% | 43.31% |  |
| 90 | 23:49:01 | 4 | `hnsw_100k_384` | hist | 1 | accepted | 808.962 | 8.06% | 48.51% |  |
| 91 | 00:02:35 | 5 | `exact_small` | cur | 1 | accepted | 0.021 | 9.55% | n/a |  |
| 92 | 00:02:40 | 5 | `exact_small` | hist | 1 | rejected | 0.178 | 15.41% | 15.41% | mean > 10% |
| 93 | 00:03:11 | 5 | `exact_small` | hist | 2 | rejected | 0.027 | 14.48% | n/a | mean > 10% |
| 94 | 00:03:26 | 5 | `exact_small` | hist | 3 | rejected | 0.028 | 32.02% | n/a | mean > 10% |
| 95 | 00:03:31 | 5 | `exact_small` | hist | 4 | accepted | 0.028 | 7.06% | n/a |  |
| 96 | 00:03:41 | 5 | `filtered_exact` | hist | 1 | rejected | 0.039 | 22.54% | n/a | mean > 10% |
| 97 | 00:03:46 | 5 | `filtered_exact` | hist | 2 | rejected | 0.039 | 30.27% | n/a | mean > 10% |
| 98 | 00:03:51 | 5 | `filtered_exact` | hist | 3 | rejected | 0.039 | 25.04% | n/a | mean > 10% |
| 99 | 00:04:01 | 5 | `filtered_exact` | hist | 4 | accepted | 0.046 | 6.44% | n/a |  |
| 100 | 00:04:06 | 5 | `filtered_exact` | cur | 1 | rejected | 0.024 | 12.15% | n/a | mean > 10% |
| 101 | 00:04:16 | 5 | `filtered_exact` | cur | 2 | rejected | 0.032 | 53.06% | n/a | mean > 10% |
| 102 | 00:04:21 | 5 | `filtered_exact` | cur | 3 | accepted | 0.026 | 3.85% | n/a |  |
| 103 | 00:04:27 | 5 | `hnsw_small` | cur | 1 | rejected | 0.032 | 28.16% | n/a | mean > 10% |
| 104 | 00:04:32 | 5 | `hnsw_small` | cur | 2 | rejected | 0.034 | 11.60% | n/a | mean > 10% |
| 105 | 00:04:37 | 5 | `hnsw_small` | cur | 3 | accepted | 0.035 | 8.41% | n/a |  |
| 106 | 00:04:42 | 5 | `hnsw_small` | hist | 1 | rejected | 0.053 | 19.36% | n/a | mean > 10% |
| 107 | 00:04:47 | 5 | `hnsw_small` | hist | 2 | rejected | 0.048 | 14.43% | n/a | mean > 10% |
| 108 | 00:04:57 | 5 | `hnsw_small` | hist | 3 | accepted | 0.046 | 6.44% | n/a |  |
| 109 | 00:05:07 | 5 | `exact_100k_384` | hist | 1 | accepted | 3.858 | 5.92% | 10.47% |  |
| 110 | 00:05:21 | 5 | `exact_100k_384` | cur | 1 | accepted | 2.043 | 3.40% | 6.31% |  |
| 111 | 00:05:28 | 5 | `filtered_100k_384` | cur | 1 | accepted | 2.273 | 2.62% | 4.65% |  |
| 112 | 00:05:46 | 5 | `filtered_100k_384` | hist | 1 | accepted | 4.307 | 8.98% | 20.93% |  |
| 113 | 00:05:55 | 5 | `hnsw_100k_384` | hist | 1 | rejected | 816.877 | 9.63% | 85.73% | sample > 50% |
| 114 | 00:19:37 | 5 | `hnsw_100k_384` | hist | 2 | rejected | 776.307 | 5.20% | 57.08% | sample > 50% |
| 115 | 00:32:44 | 5 | `hnsw_100k_384` | hist | 3 | accepted | 778.551 | 6.43% | 29.45% |  |
| 116 | 00:45:47 | 5 | `hnsw_100k_384` | cur | 1 | accepted | 789.129 | 6.51% | 32.02% |  |

"bg max sample" is n/a for runs under 0.1 s: the monitor does not count a sample that short for its peak rule.

## Appendix B — selection effect of rejecting runs (informational; rejected runs are excluded from every statistic above)
Median ingest (vectors/s) of accepted runs, of rejected runs that completed, and of all completed attempts.

| Workload/revision | accepted | rejected (excluded) | all completed attempts | all vs accepted |
| :-- | --: | --: | --: | --: |
| `exact_small/hist` | 53,108 (n=5) | 52,390 (n=6) | 52,710 (n=11) | -0.7% |
| `exact_small/cur` | 49,601 (n=5) | 53,191 (n=5) | 51,102 (n=10) | +3.0% |
| `filtered_exact/hist` | 143,135 (n=5) | 131,833 (n=6) | 139,717 (n=11) | -2.4% |
| `filtered_exact/cur` | 149,561 (n=5) | 146,518 (n=5) | 146,737 (n=10) | -1.9% |
| `hnsw_small/hist` | 156,088 (n=5) | 149,214 (n=8) | 150,988 (n=13) | -3.3% |
| `hnsw_small/cur` | 159,770 (n=5) | 154,422 (n=11) | 156,091 (n=16) | -2.3% |
| `exact_100k_384/hist` | 66,710 (n=5) | 65,067 (n=1) | 65,933 (n=6) | -1.2% |
| `exact_100k_384/cur` | 156,248 (n=5) | – | 156,248 (n=5) | +0.0% |
| `filtered_100k_384/hist` | 59,904 (n=5) | 60,736 (n=1) | 60,204 (n=6) | +0.5% |
| `filtered_100k_384/cur` | 124,206 (n=5) | – | 124,206 (n=5) | +0.0% |
| `hnsw_100k_384/hist` | 66,210 (n=4) | 66,158 (n=7) | 66,158 (n=11) | -0.1% |
| `hnsw_100k_384/cur` | 161,514 (n=4) | 159,065 (n=8) | 160,189 (n=12) | -0.8% |


## Appendix C — every accepted run (the values behind the medians)

### `exact_small`

| rep | rev | try | Ingest (vectors/s) | Exact query p50 (ms) | Exact query p99 (ms) | Reopen (ms) | Peak working set (MiB) | Exact recall@k | bg mean |
| --: | :-- | --: | --: | --: | --: | --: | --: | --: | --: |
| 1 | cur | 1 | 57,637 | 0.0020 | 0.0025 | 0.1550 | 4.906 | 1 | 5.80% |
| 1 | hist | 1 | 52,710 | 0.0018 | 0.0025 | 0.1858 | 4.855 | 1 | 2.90% |
| 2 | cur | 3 | 49,601 | 0.0020 | 0.0026 | 0.1599 | 4.914 | 1 | 7.10% |
| 2 | hist | 3 | 54,045 | 0.0019 | 0.0023 | 0.1934 | 4.852 | 1 | 3.68% |
| 3 | cur | 1 | 45,374 | 0.0030 | 0.0035 | 0.2088 | 4.680 | 1 | 9.11% |
| 3 | hist | 1 | 53,108 | 0.0018 | 0.0024 | 0.1900 | 4.621 | 1 | 6.62% |
| 4 | cur | 4 | 50,943 | 0.0020 | 0.0025 | 0.1507 | 4.688 | 1 | 6.16% |
| 4 | hist | 2 | 57,281 | 0.0030 | 0.0066 | 0.1775 | 4.625 | 1 | 7.38% |
| 5 | cur | 1 | 47,450 | 0.0020 | 0.0039 | 0.1810 | 4.688 | 1 | 9.55% |
| 5 | hist | 4 | 43,472 | 0.0018 | 0.0025 | 0.1870 | 4.625 | 1 | 7.06% |


### `filtered_exact`

| rep | rev | try | Ingest (vectors/s) | Exact query p50 (ms) | Exact query p99 (ms) | Reopen (ms) | Peak working set (MiB) | Exact recall@k | bg mean |
| --: | :-- | --: | --: | --: | --: | --: | --: | --: | --: |
| 1 | cur | 3 | 143,982 | 0.0228 | 0.0267 | 1.059 | 5.492 | 1 | 5.51% |
| 1 | hist | 1 | 146,006 | 0.0203 | 0.0226 | 1.331 | 5.422 | 1 | 7.63% |
| 2 | cur | 1 | 155,173 | 0.0238 | 0.0477 | 0.9861 | 5.496 | 1 | 2.77% |
| 2 | hist | 4 | 140,165 | 0.0200 | 0.0523 | 1.463 | 5.422 | 1 | 8.27% |
| 3 | cur | 1 | 149,561 | 0.0226 | 0.0271 | 1.007 | 5.266 | 1 | 3.90% |
| 3 | hist | 1 | 146,751 | 0.0196 | 0.0213 | 1.415 | 5.188 | 1 | 2.73% |
| 4 | cur | 2 | 150,131 | 0.0228 | 0.0247 | 0.9912 | 5.258 | 1 | 8.21% |
| 4 | hist | 1 | 143,135 | 0.0193 | 0.0205 | 1.366 | 5.195 | 1 | 4.96% |
| 5 | cur | 3 | 144,897 | 0.0223 | 0.0257 | 0.9811 | 5.273 | 1 | 3.85% |
| 5 | hist | 4 | 69,428 | 0.0193 | 0.0209 | 1.421 | 5.195 | 1 | 6.44% |


### `hnsw_small`

| rep | rev | try | Ingest (vectors/s) | Exact query p50 (ms) | Exact query p99 (ms) | Reopen (ms) | Peak working set (MiB) | Exact recall@k | HNSW index build (ms) | HNSW query p50 (ms) | HNSW query p99 (ms) | HNSW recall@k | bg mean |
| --: | :-- | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: |
| 1 | cur | 5 | 161,341 | 0.0059 | 0.0065 | 0.2407 | 5.000 | 1 | 13.997 | 0.0409 | 0.0431 | 1 | 8.45% |
| 1 | hist | 1 | 148,578 | 0.0054 | 0.0060 | 0.2696 | 4.984 | 1 | 15.752 | 0.0400 | 0.0420 | 1 | 3.96% |
| 2 | cur | 4 | 164,071 | 0.0058 | 0.0066 | 0.2649 | 4.992 | 1 | 13.938 | 0.0411 | 0.0430 | 1 | 6.61% |
| 2 | hist | 2 | 156,088 | 0.0053 | 0.0058 | 0.3294 | 4.988 | 1 | 15.804 | 0.0401 | 0.0429 | 1 | 6.53% |
| 3 | cur | 3 | 159,770 | 0.0058 | 0.0063 | 0.2295 | 4.766 | 1 | 13.819 | 0.0416 | 0.0612 | 1 | 9.32% |
| 3 | hist | 5 | 164,514 | 0.0054 | 0.0067 | 0.2824 | 4.758 | 1 | 15.778 | 0.0403 | 0.0530 | 1 | 8.68% |
| 4 | cur | 1 | 142,825 | 0.0058 | 0.0065 | 0.2295 | 4.773 | 1 | 14.044 | 0.0426 | 0.0704 | 1 | 8.15% |
| 4 | hist | 2 | 157,713 | 0.0054 | 0.0058 | 0.2918 | 4.762 | 1 | 15.793 | 0.0402 | 0.0485 | 1 | 6.46% |
| 5 | cur | 3 | 156,795 | 0.0059 | 0.0064 | 0.2322 | 4.766 | 1 | 14.011 | 0.0421 | 0.0791 | 1 | 8.41% |
| 5 | hist | 3 | 149,472 | 0.0054 | 0.0058 | 0.2702 | 4.758 | 1 | 15.533 | 0.0410 | 0.0483 | 1 | 6.44% |


### `exact_100k_384`

| rep | rev | try | Ingest (vectors/s) | Exact query p50 (ms) | Exact query p99 (ms) | Reopen (ms) | Peak working set (MiB) | Exact recall@k | bg mean |
| --: | :-- | --: | --: | --: | --: | --: | --: | --: | --: |
| 1 | cur | 1 | 169,151 | 9.023 | 10.973 | 218.0 | 326.0 | 1 | 4.32% |
| 1 | hist | 1 | 61,485 | 8.936 | 9.334 | 1,202 | 328.5 | 1 | 4.66% |
| 2 | cur | 1 | 161,171 | 8.406 | 9.352 | 227.6 | 326.0 | 1 | 4.65% |
| 2 | hist | 2 | 67,593 | 8.870 | 10.188 | 1,156 | 328.5 | 1 | 5.96% |
| 3 | cur | 1 | 156,248 | 8.695 | 9.946 | 232.7 | 325.8 | 1 | 2.42% |
| 3 | hist | 1 | 66,937 | 9.030 | 9.399 | 1,172 | 328.2 | 1 | 4.67% |
| 4 | cur | 1 | 154,260 | 8.890 | 9.828 | 236.3 | 325.9 | 1 | 7.02% |
| 4 | hist | 1 | 66,710 | 8.933 | 9.930 | 1,167 | 328.2 | 1 | 7.10% |
| 5 | cur | 1 | 153,324 | 8.719 | 10.630 | 231.6 | 325.8 | 1 | 3.40% |
| 5 | hist | 1 | 65,155 | 9.447 | 13.229 | 1,177 | 328.2 | 1 | 5.92% |


### `filtered_100k_384`

| rep | rev | try | Ingest (vectors/s) | Exact query p50 (ms) | Exact query p99 (ms) | Reopen (ms) | Peak working set (MiB) | Exact recall@k | bg mean |
| --: | :-- | --: | --: | --: | --: | --: | --: | --: | --: |
| 1 | cur | 1 | 125,881 | 5.584 | 8.161 | 266.7 | 382.3 | 1 | 3.56% |
| 1 | hist | 1 | 61,710 | 5.253 | 5.406 | 1,214 | 385.0 | 1 | 4.92% |
| 2 | cur | 1 | 123,374 | 5.263 | 5.992 | 279.4 | 382.3 | 1 | 9.16% |
| 2 | hist | 2 | 60,505 | 5.336 | 5.934 | 1,221 | 385.0 | 1 | 9.90% |
| 3 | cur | 1 | 124,206 | 5.813 | 6.169 | 280.9 | 382.0 | 1 | 1.54% |
| 3 | hist | 1 | 59,904 | 5.533 | 5.850 | 1,213 | 384.8 | 1 | 5.37% |
| 4 | cur | 1 | 125,075 | 5.751 | 8.013 | 276.0 | 382.0 | 1 | 2.77% |
| 4 | hist | 1 | 59,641 | 5.532 | 5.813 | 1,223 | 384.8 | 1 | 5.59% |
| 5 | cur | 1 | 117,197 | 5.628 | 5.675 | 285.4 | 382.0 | 1 | 2.62% |
| 5 | hist | 1 | 52,401 | 5.548 | 7.047 | 1,233 | 384.8 | 1 | 8.98% |


### `hnsw_100k_384`

| rep | rev | try | Ingest (vectors/s) | Exact query p50 (ms) | Exact query p99 (ms) | Reopen (ms) | Peak working set (MiB) | Exact recall@k | HNSW index build (ms) | HNSW query p50 (ms) | HNSW query p99 (ms) | HNSW recall@k | bg mean |
| --: | :-- | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: | --: |
| 1 | cur | 3 | 164,289 | 8.994 | 9.529 | 228.1 | 353.4 | 1 | 761,910 | 3.116 | 3.724 | 0.26 | 6.95% |
| 1 | hist | 1 | 66,372 | 8.275 | 10.424 | 1,165 | 355.8 | 1 | 754,832 | 2.835 | 3.465 | 0.26 | 5.46% |
| 2 | cur | 3 | 166,767 | 8.277 | 10.540 | 221.9 | 353.4 | 1 | 757,428 | 3.120 | 4.006 | 0.26 | 7.39% |
| 3 | hist | 3 | 65,566 | 8.852 | 9.501 | 1,168 | 355.6 | 1 | 778,528 | 3.018 | 3.611 | 0.26 | 5.34% |
| 4 | cur | 2 | 158,739 | 8.739 | 9.935 | 231.5 | 353.2 | 1 | 795,119 | 3.288 | 4.037 | 0.26 | 5.68% |
| 4 | hist | 1 | 66,047 | 9.130 | 9.443 | 1,167 | 355.5 | 1 | 804,562 | 3.094 | 3.893 | 0.26 | 8.06% |
| 5 | cur | 1 | 155,570 | 8.719 | 9.656 | 232.4 | 353.2 | 1 | 786,539 | 3.237 | 3.915 | 0.26 | 6.51% |
| 5 | hist | 3 | 66,829 | 9.216 | 10.480 | 1,175 | 355.7 | 1 | 774,184 | 3.113 | 3.784 | 0.26 | 6.43% |


## Appendix D — bursts above the 50 % per-sample limit
175 samples in 15 attempts form 20 bursts: consecutive over-limit samples of one attempt less than 10 s apart (any outcome; each of those attempts fails the unchanged 50 % per-sample limit). "UTC start" is the approximate start of the burst (attempt start + pre-run wait + cumulative sample time, about ±0.2 s); "length" runs from the start of its first to the end of its last over-limit sample; "samples over" counts the samples above the limit; "gap" is the time since the previous burst ended. The last two columns name the call of that kind closest in time to the burst ("during": a moment the call was active lies inside it). Every individual sample is in `comparison-report.json` (`burst_samples_above_limit`) and in the monitor samples of `raw/<tag>/runs.jsonl`. The table is evidence for the reader; no cause is asserted.

| UTC start (MM-DD HH:MM:SS) | length s | rep | workload | rev | try | outcome | samples over | peak | longest sample s | gap since previous burst (min) | nearest process-starting call | nearest in-process call |
| :-- | --: | --: | :-- | :-- | --: | :-- | --: | --: | --: | --: | :-- | :-- |
| 10-25 18:50:51 | 37.8 | 1 | `hnsw_100k_384` | cur | 1 | rejected | 41 | 97.85% | 0.54 | – | none within 60 s | none within 60 s |
| 10-25 19:01:50 | 0.5 | 1 | `hnsw_100k_384` | cur | 2 | rejected | 1 | 52.64% | 0.51 | 10.4 | none within 60 s | none within 60 s |
| 10-25 19:27:32 | 0.5 | 2 | `hnsw_100k_384` | cur | 1 | rejected | 1 | 51.92% | 0.50 | 25.7 | none within 60 s | none within 60 s |
| 10-25 19:42:11 | 11.2 | 2 | `hnsw_100k_384` | cur | 2 | rejected | 3 | 114.53% | 0.53 | 14.7 | none within 60 s | none within 60 s |
| 10-25 19:42:37 | 22.2 | 2 | `hnsw_100k_384` | cur | 2 | rejected | 9 | 167.23% | 0.53 | 0.2 | none within 60 s | none within 60 s |
| 10-25 19:43:17 | 9.2 | 2 | `hnsw_100k_384` | cur | 2 | rejected | 2 | 62.06% | 0.52 | 0.3 | none within 60 s | none within 60 s |
| 10-25 19:43:48 | 3.1 | 2 | `hnsw_100k_384` | cur | 2 | rejected | 2 | 58.48% | 2.04 | 0.4 | `powershell` 58.0 s after | none within 60 s |
| 10-25 20:12:21 | 2.4 | 2 | `hnsw_100k_384` | hist | 1 | rejected (stalled) | 1 | 80.44% | 2.39 | 28.5 | stalled attempt, not analysed | stalled attempt, not analysed |
| 10-25 21:26:56 | 94.3 | 2 | `hnsw_100k_384` | hist | 1 | rejected (stalled) | 88 | 116.22% | 0.87 | 74.5 | stalled attempt, not analysed | stalled attempt, not analysed |
| 10-25 21:34:47 | 1.0 | 2 | `hnsw_100k_384` | hist | 2 | rejected | 2 | 93.64% | 0.51 | 6.3 | `powershell` 14.1 s after | `create` 0.7 s before |
| 10-25 21:36:56 | 0.5 | 2 | `hnsw_100k_384` | hist | 2 | rejected | 1 | 53.90% | 0.50 | 2.1 | none within 60 s | none within 60 s |
| 10-25 21:57:01 | 0.5 | 2 | `hnsw_100k_384` | hist | 3 | rejected | 1 | 52.03% | 0.51 | 20.1 | none within 60 s | none within 60 s |
| 10-25 22:02:48 | 1.0 | 3 | `hnsw_100k_384` | hist | 1 | rejected | 2 | 94.18% | 0.51 | 5.8 | none within 60 s | none within 60 s |
| 10-25 22:21:57 | 2.0 | 3 | `hnsw_100k_384` | hist | 2 | rejected | 4 | 94.08% | 0.52 | 19.1 | none within 60 s | none within 60 s |
| 10-25 22:51:02 | 1.5 | 3 | `hnsw_100k_384` | cur | 1 | rejected | 3 | 93.71% | 0.52 | 29.1 | `powershell` 27.6 s after | none within 60 s |
| 10-25 22:56:54 | 1.0 | 3 | `hnsw_100k_384` | cur | 2 | rejected | 2 | 58.03% | 0.51 | 5.8 | `grep` 12.6 s before | `view` during the burst |
| 10-25 23:09:41 | 1.0 | 3 | `hnsw_100k_384` | cur | 3 | rejected | 2 | 93.45% | 0.50 | 12.8 | none within 60 s | none within 60 s |
| 10-25 23:26:54 | 0.5 | 4 | `hnsw_100k_384` | cur | 1 | rejected | 1 | 51.52% | 0.50 | 17.2 | `grep` 2.7 s after | `view` during the burst |
| 10-26 00:13:05 | 15.8 | 5 | `hnsw_100k_384` | hist | 1 | rejected | 8 | 85.73% | 0.52 | 46.2 | none within 60 s | none within 60 s |
| 10-26 00:28:52 | 0.5 | 5 | `hnsw_100k_384` | hist | 2 | rejected | 1 | 57.08% | 0.51 | 15.5 | none within 60 s | none within 60 s |

