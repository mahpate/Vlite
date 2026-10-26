# DO-005 graph-off — separate-host campaign: handoff

**Temporary results branch.** `mahpate-do005-sephost-handoff` exists only to hand these results to another session. It is not meant to be merged; delete it when done (`git push origin --delete mahpate-do005-sephost-handoff`). Its only change from `origin/main` `adde71b91c9e865d4bc08628717cca7d72f7baa0` is this folder. CI was skipped for the commit (`[skip ci]`).

**Not a baseline.** This is a separate-host comparison of historical `81ff901` against `origin/main` pinned at `c024dce`. It does not replace or amend the original host's baseline. `baseline.md` and the other baseline documents are untouched, and no gate was evaluated or approved.

The report's statement "nothing was committed or pushed" was true when it was generated (2026-10-26T01:18:40Z). This folder is a copy pushed afterwards at the user's request. Report and evidence were not regenerated; the only edits are the redactions listed below.

Originating Copilot session: `7d6b23a0-e3e5-4941-8e92-56286ce55f36`.

## Start here
1. `report/comparison-report.md` — the full report: outcome, results, CPU policy, quiet-window evidence, host identity, provenance, protocol checks, limitations, and appendices with every attempt and every accepted run. `report/comparison-report.json` is its machine-readable twin.
2. This file — summary, caveats, redactions, verification and next steps.
3. `handoff/operator-activity-notes.md` — notes taken during the run about agent activity versus rejected runs.

## Pins and protocol

| Revision | Commit | Tree |
| :-- | :-- | :-- |
| historical | `81ff901c54d674d1ad519dd4fa722e937db46c4b` | `da11989ef426f7f41835616300670e7b60928b9a` |
| current `origin/main` (pinned 2026-10-25T17:37:09Z) | `c024dcea72f1479e2114254e6d6388dcb5657315` | `96a35ceb7fa211dc78c6839ceb2a20fe49940b8a` |

An earlier fetch (`41cb396b0bc5`) was superseded before anything was built.

**Sources and builds**
- Each revision was built from its own clean `git archive`. Both archives were LF-normalised the same way and verified against the git blobs.
- Builds were release, default features (graph off), `vlite-bench`.
- One online `cargo fetch` ran per revision before the quiet window; every build after that was locked and offline.

**Toolchain:** rustc and cargo 1.99.1, target `x86_64-pc-windows-msvc`, MSVC 14.51.36231 linker, Python 3.15.0. Both revisions used the same toolchain.

**Workloads**
- Smoke: `exact_small`, `filtered_exact`, `hnsw_small`.
- Reference: `exact_100k_384`, `filtered_100k_384`, `hnsw_100k_384`.
- 5 repetitions of each per revision, with identical arguments and one storage directory on a `Q:` ReFS Dev Drive.
- Revision order alternated within each pair.

**CPU acceptance policy (unchanged):** background mean ≤ 10 %, and every sample of at least 0.1 s ≤ 50 %, with scanner/System CPU counted as background.
- Every measured run went through the pinned Windows monitor, `graph_reference.run_process` (sha256 `c520e0b759c16d6c…`). Its constants are asserted when it loads.
- `graph_baseline.py measure` was not relied on, because it does not import the monitor (report §7).

**Host:** a Hyper-V VM with an AMD EPYC 9V74 (8 cores, 16 logical), 68,714,033,152 bytes of RAM, and Windows 11 Enterprise 10.0.26300 (26H2, UBR 9711). It is a **different machine** from the reference host, which has an Intel Xeon Platinum 8370C.

**Measurement window:** 2026-10-25 18:30:03Z to 2026-10-26 00:59:01Z.

## Outcome
**INCOMPLETE.** 58 of 60 slots (6 workloads × 2 revisions × 5 repetitions) have an accepted run. There were 58 rejected attempts, 0 refused, 0 not attempted and 0 quarantined.

Two slots have no accepted run. Each used all 3 allowed attempts, and the CPU policy rejected every one:
- `hnsw_100k_384` historical, repetition 2
- `hnsw_100k_384` current, repetition 3

**Headline results.** These are medians of accepted runs, current versus historical; n = 5, except n = 4 for `hnsw_100k_384`.

| Workload | Ingest (vectors/s) | Reopen | Exact query p50 | Other |
| :-- | :-- | :-- | :-- | :-- |
| `exact_100k_384` | 66,710 → 156,248 (+134.2 %) | 1,172 → 231.6 ms (−80.2 %) | −2.4 % (overlap) | child wall time −46.9 % |
| `filtered_100k_384` | 59,904 → 124,206 (+107.3 %) | 1,221 → 279.4 ms (−77.1 %) | +1.7 % (overlap) | child wall time −44.7 % |
| `hnsw_100k_384` | 66,210 → 161,514 (+143.9 %) | 1,167 → 229.8 ms (−80.3 %) | −2.9 % (overlap) | HNSW build 776,356 → 774,224 ms (−0.3 %, overlap); HNSW query p50 +4.0 % (disjoint), p99 +7.1 % (overlap) |

- For ingest and reopen, the historical and current ranges are disjoint in all three workloads.
- Peak working set is −0.7 % in all three, and recall is unchanged.
- The smoke-workload results are in report §2. Their CPU check is weak (see caveats).

## Caveats — read before using the numbers
- **Different machine.** The CPU and the RAM size differ from the reference host, so the identity checks `cpu_is_xeon_8370c` and `ram_bytes_equal_reference` fail. These results are not comparable with the original baseline for REG-010 purposes (report §5; §9 is informational only).
- **Noisy host.**
  - A 599 s idle survey had a mean of 12.03 % and a worst sample of 63.25 %.
  - The organisation-managed security and management agents could not be paused without administrator rights.
  - 15 of the 23 `hnsw_100k_384` attempts were rejected, all on the 50 % per-sample limit alone.
- **VM stall.** The guest did not run for about 74 minutes, from 2026-10-25 20:12:25Z to 21:26:50Z. The cause is unconfirmed.
  - The affected attempt was rejected.
  - The report quarantines any accepted run with a monitor sample over 5 s; no run met that condition.
  - Details: `evidence/window/incident-vm-freeze.json` and report §4.
- **Agent activity overlapped the runs.** The agent session running the campaign was not idle during the measurement window. It made 205 tool calls (77 of them started a process) across 148 model turns.
  - 71 process-starting and 113 in-process calls overlapped measured runs, so the exclusive-quiet-window requirement is only **partial**.
  - In the long runs, background CPU averaged 8.35 % within ±5 s of a process-starting call and 6.51 % away from any activity.
  - Outside the stalled attempt, 1 of the 18 bursts over 50 % coincided with a process-starting call; 1.0 would be expected by chance.
  - The timing evidence neither establishes nor excludes a contribution to any rejection. Details: report §4, §10 and Appendix D, and `evidence/window/agent-activity.json`.
- **Short smoke runs.** These finish in tens of milliseconds, so the per-sample CPU check is nearly vacuous for them. Retrying until a run passes is also a selection effect (report §10, Appendix B).
- **`origin/main` has moved.** At report time the head was `adde71b91c9e`, 9 commits after the pin.
  - The report marks **REVIEW NEEDED**: 11 changed paths cannot be shown to lie outside the default-feature `vlite-bench` build.
  - The newer head was not built or measured (report §7).
- **n ≤ 5.** The report gives medians and ranges only and claims no significance.

## What is in this folder

| Path | Contents |
| :-- | :-- |
| `report/` | `comparison-report.md` and `.json`; `ARTIFACTS.sha256` (SHA-256 of the 64 evidence, raw, runner and report files as generated); `ARTIFACTS.rollup.txt` (`2fee10fc799b4952058c3bc0a6ee7925c91db85a3135bb7449a9b26b131b5661`) |
| `raw/` | `main/runs.jsonl`: one record per attempt, accepted or rejected, holding the child's complete stdout and stderr, every monitor sample, the pre-run probes and the process censuses. Also `campaign.log`, `campaign-config.json`, the start/end censuses and source re-verifications, and dry-run/self-test data (excluded from all statistics) |
| `evidence/` | Pins, archives, toolchain, builds and cargo logs, host, reference-identity checks, runner and pinned-script hashes, self-test, quiet-window surveys, app-session snapshots, agent activity, the VM-stall incident |
| `runner/` | `do005_campaign.py`, `do005_common.py` and `do005_prepare.py` (frozen during measurement; hashes in report §6), plus `do005_report.py` (the post-processing report generator) |
| `handoff/` | `operator-activity-notes.md`, and `crosscheck.py`, an independent QA script that recomputes the CPU policy, slot accounting, medians, determinism, hashes, git state, agent-activity coincidence and the artifact index |
| `REDACTIONS.sha256` | SHA-256 of the 7 redacted copies |
| `.gitattributes` | `* -text`, so git stores and checks out every file byte-for-byte, even with `core.autocrlf=true` |

**Not included**, because they are too large or reproducible. All remain on the measurement host under `Q:\vlite-campaign\do005-sephost-20261025T172902Z`:
- `src/`: the two source archives, 902 files, 54 MB. They are reproducible from the pins; the tar hashes are in report §6.
- `target/`: build output, 1,192 files, 361 MB. The binary hashes are in report §6: `vlite-bench` historical `2ed6b33838cabd92…`, current `aad2c49f1893ebbd…`.
- `work/`: an empty scratch folder.

Also on that host: the toolchain at `Q:\vlite-campaign\toolchain`, and the clean clone used as the archive source at `Q:\vlite`.

## Redactions (this repository is public)
Only the copies in this folder were redacted. The unredacted originals are unchanged on the measurement host, and their hashes are the ones in `report/ARTIFACTS.sha256`.

- Each redaction is a plain string substitution; nothing else in these files changed. The redacted JSON still parses, and the Python files still parse.
- The original campaign had already replaced the Windows machine GUID with its hash in `evidence/host.json`.
- Session IDs, statuses and counts are kept; only the session names were removed.
- The username `mahpate`, who owns this repository, was not redacted.

| File | Replaced | Original sha256 | Published sha256 |
| :-- | :-- | :-- | :-- |
| `evidence/host.json` | VM hostname ×2 → `[REDACTED-HOSTNAME]`; BIOS serial / identifying number ×2 → `[REDACTED-BIOS-SERIAL]`; SMBIOS UUID ×1 → `[REDACTED-MACHINE-UUID]` | `50b038252105cd39…` | `6ab3af989774ddda…` |
| `evidence/window/sessions-pre.json` | names of the user's 5 other Copilot app sessions → `[REDACTED other session name 1]`…`5` | `65400095088c3618…` | `005acb609e796cf9…` |
| `evidence/window/sessions-post.json` | the same 5 names | `28bb2cda39f62bad…` | `afb5a953fda6ce74…` |
| `report/comparison-report.md` | VM hostname ×2 | `af76dee147f98493…` | `a8af647c4f846b77…` |
| `report/comparison-report.json` | the 5 session names ×2 each; an internal folder name in the agent worktree path ×1 → `[REDACTED-DIR]` | `54f0fa7bfc13b291…` | `4affca11e65c3c6f…` |
| `runner/do005_report.py` | internal folder name ×1 | `81c7cf259ee39e7b…` | `14449f6a1b2a5ab5…` |
| `handoff/crosscheck.py` | internal folder name ×1 | `0997bc1995f6d6f7…` (not indexed) | `0311dfc4374b78ad…` |

## How to verify
From this folder, in Git Bash or any shell that has `sha256sum`:

```sh
sha256sum report/ARTIFACTS.sha256             # 2fee10fc…b5661, the value in report/ARTIFACTS.rollup.txt
sha256sum -c --quiet report/ARTIFACTS.sha256  # expected: only the 6 redacted indexed files fail
sha256sum -c REDACTIONS.sha256                # expected: 7 OK
```

`handoff/crosscheck.py` hardcodes the measurement host's paths (`CAMP`, `CLONE`, `WORKTREE`).
- **Before redaction:** run on the measurement host against the unredacted originals, it reported 0 failures when the report was finalised.
- **After redaction:** run against this folder (paths repointed in memory), it reported 83 ok and 2 expected failures:
  - H: the 6 redacted files.
  - F: the folder was not yet committed at that moment.
- Section E reads the binaries and source archives by their absolute paths, so it fails on any other machine.

## Re-running anything
- The runner and report scripts hardcode the measurement host's absolute paths (`Q:\vlite-campaign\…`, `Q:\vlite`). They need the toolchain and the `src/` and `target/` folders, none of which were pushed. Run Python as `Q:\vlite-campaign\toolchain\python\python.exe -B`.
- Do not regenerate `comparison-report.*`. Its git-state check would now see this branch, and the artifact hashes would change.

## Suggested next steps for the receiving session
1. **Optional top-up of the 2 missing `hnsw_100k_384` slots.**
   - The frozen runner caps that workload at 3 attempts per slot. The cap is an operational time bound, not a quality threshold (report §1).
   - A top-up therefore needs a separate, documented pass with a raised cap. Report it as a supplement with its own runner hash, and leave the existing report unedited.
   - Each attempt takes about 13.4 minutes: at least 27 minutes for both slots, or about 80 minutes if each slot needs 3 attempts.
   - Keep the session **strictly hands-off** while runs are in progress: no tool calls and no polling.
2. **Newer `origin/main`.** If that is the revision to compare, re-pin, re-archive, rebuild and re-measure both revisions with the same protocol.
3. Keep the original host's baseline separate. Do not edit `baseline.md` or `graph-measurements.md` from these results, and do not approve gates with them.
4. Delete this branch when the handoff is complete.
