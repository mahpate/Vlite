# Operator activity vs. measured runs (must be disclosed in the report)

Found 2026-10-25 ~23:43Z while the campaign was running (do005-sephost-20261025T172902Z, tag main).

- Rep 4 hnsw_100k_384 **cur** attempt 1 (child start ~23:22:25Z) was rejected: mean 5.70%, one sample 51.52% (limit 50%).
  - Over-30% samples of that run (offset s from child start / UTC / background): 267.6 / 23:26:53 / 34.88%; 269.1 / 23:26:54 / **51.52%**;
    277.8 / 23:27:03 / 32.77%; 296.6 / 23:27:22 / 30.36%; 481.1 / 23:30:26 / 34.67%.
  - Operator commands near those times: a Python snippet that read all of raw/main/runs.jsonl at ~23:26:5x; a PowerShell poll started 23:27:24;
    a PowerShell poll + a Python snippet that read all of runs.jsonl at ~23:30:31.
  - Reading: operator-started processes and big-file reads produce ~30-50% single-sample bursts on this host (Defender/System scanning, process start).
    The 51.52% sample is probably operator-induced; this is an inference, not proven. The run stays rejected (conservative).
- Earlier belief "small blips during HNSW runs are acceptable" was WRONG for this host. Rule from now on: during measured runs start no processes,
  read no large files, run no grep/rg over evidence; only rare async Start-Sleep polls that tail campaign.log in-process.
- During rep 4 cur attempt 2 (started 23:35:38Z) I again ran: PowerShell polls/launches (~23:36:30-23:37:30), a ripgrep over evidence\ (~23:38-23:40),
  Get-Item (~23:39), python ast.parse (23:41:00), a python snippet reading all of runs.jsonl (~23:30 and ~23:42).
  If attempt 2 is rejected, check its over-limit samples against these times.

## To do after the campaign
1. Reconstruct the exact operator command times from the session event log (C:\Users\mahpate\.copilot\session-state\<id>\events.jsonl, if present)
   - list tool executions (powershell/grep/glob/view/edit) between the campaign start (18:23:02Z) and end.
2. Correlate every over-limit (>50%) and >30% sample in runs.jsonl with the nearest operator action (+-30 s); report counts, no causal claim beyond coincidence.
3. Add to the report: a limitation "Operator activity" + an appendix with the coincidence table; say plainly that operator processes count as background CPU
   (policy requires it), that rejected runs near operator actions stay rejected, and that accepted runs may contain small operator blips within the policy.
4. Mention it in the final answer.
