# HP-1: WIP, stopped at an undetermined byte rule

Follow-up: the source-scope interpretation behind the original stop below
was too narrow. The general graph codec is not the forbidden subset
implementation and supplies body/entry framing. See the corrected candidate
report and `hp1-gap6-write-oracle-status.md`. The initial candidate SHA below
is historical, not a completed or accepted implementation.

Candidate: `9f24d0217f46692f62a8dcd2adb31440f0cb4362`.
Base: `136d72c5a94e1c0d1878f99c461a555a91b7c3f6`.
Handover verified: `eb9c77b672320cf8b74711ac5aa6fa998adfc753`.
Branch: `mahpate-hp1-gap6-write-oracle`. Do not integrate this report commit.

## Result and resume prerequisite

**Not complete.** The explicit undetermined-byte stop rule was reached before
implementation. OPEN-W01 is documented in the candidate's only new file,
`tests/graph_subset_write_report.md` (106 lines).
This final report is the only additional file (65 lines).

The design fixes a 16,777,216-byte encoded-body cap including reclaims.
Permitted sources delegate its actual encoding to code outside the authorized
ground-truth reading scope: `mod.rs:107,635-661,777-788` and
`engine/state.rs:835-839`. Metadata lengths and a frame-prefix lower bound
cannot establish complete operation/body encoding or exact-cap expectations.
No byte ruling was invented; no forbidden subset implementation was read.

Resume only after an independently frozen operation-to-body byte contract
is supplied, or additional evidence access is explicitly authorized and
independence reviewed. Required details and proposed cap/over-cap cases are
in OPEN-W01. This is not a claimed design contradiction or implementation bug.

No write model, fixtures, counters, state transition, 150-case corpus, or
write mutation campaign was delivered. Write mutants: killed 0, survived 0,
equivalent 0; none implemented or run, so no equivalence claim.
All implementation and executable-write coverage remains unfinished.
Deadline/hold-time/scratch execution cost, cancellation, I/O and durability
remain unmodeled. Python 3.13.17; Git 2.56.0.windows.4.
Rust tools unavailable on PATH; no installation/workaround/build attempted.

Only the isolated checkout was used. No existing files, other packages,
implementation, CI, status or public documentation changed. Only this branch
is pushed. The prompt's exact final report path is used instead of the
generic handover's WIP `-status.md` suffix. No false passing placeholder
was created. Initial CRLF/check failures were corrected before delivery.

## Proposed CI, after actual completion only

Oracle-list commands in `.github/workflows/ci.yml`:
`python -B tests/graph_subset_write_oracle.py --self-test` and
`python -O -B tests/graph_subset_write_oracle.py --self-test`.
Name in `tests/ci_workflows.py`: `graph_subset_write_oracle.py`.
Do not enable these against this WIP.

## Evidence

Commands ran from the isolated checkout root; paths below are relative.

- `git fetch origin mahpate-pool-briefs-w4`: 0.
- `git rev-parse origin/mahpate-pool-briefs-w4`: 0; pinned SHA above.
- `python -B tests\graph_subset_oracle.py --self-test`: 0.
- `python -O -B tests\graph_subset_oracle.py --self-test`: 0.
  Each: 303 parse/218 read cases; 63 parser/70 read switches killed;
  79 named read kills; 36/38 malformed, 14/75 boundary trials;
  800 fuzz trials; five shuffle seeds per suite.
- `python -B tests\graph_subset_write_oracle.py --self-test`: 2, missing file.
- `python -O -B tests\graph_subset_write_oracle.py --self-test`: 2, missing file.
- `git diff --cached --check`: initially 2 (CRLF), then 0 after LF normalization.
- `git diff --check 136d72c5a94e1c0d1878f99c461a555a91b7c3f6..HEAD`: 0.
- `git diff --name-status 136d72c5a94e1c0d1878f99c461a555a91b7c3f6..HEAD`:
  0; candidate has one `A` path; final delivery has only two `A` paths.
