# HP-5 status: blocked at environment gate

Status: WIP; no oracle or fixtures authored.

Branch: `mahpate-hp5-p3c-ann-oracle`.
Base and candidate SHA: `136d72c5a94e1c0d1878f99c461a555a91b7c3f6`.
The candidate is the unchanged base, not an implementation candidate.
This separate final report commit is not integrable.

## Completed

Fetched origin, switched to the handover branch, pulled with fast-forward
only, and read the complete handover at
`813d89b` including sections 0-4, HP-5, section 7 and all appended updates.
Created the requested HP-5 branch directly from the required base.
Started the first prescribed design read; no ANN implementation was read.
No existing files, other packages, shared code, CI or public documentation
were changed.

## OPEN-ENV-01: conflicting environment instructions

Section 5.6 and its start prompt explicitly describe HP-5 as Python-only.
Section 7 nevertheless prescribes checking Git, Python, rustc and cargo,
and says to stop and report if a tool is missing. Both Rust commands are
unavailable on PATH. Section 0.6 requires the stricter rule when instructions
disagree, so work stopped rather than installing tools, changing PATH or
silently exempting HP-5.

Proposed coordinator ruling: clarify that section 7's Rust checks and
missing-Rust stop condition apply only to packages requiring Rust. Otherwise,
provide an environment with the prescribed tools. No ruling is assumed here.

## Remaining and resumption

All section 5.6 deliverables, byte-contract review, hand calculations,
120-case coverage target, mutation campaign and regression validation remain
unstarted. Nothing is modeled, frozen, qualified or accepted. Mutation counts
are not measured; no killed, surviving or equivalent mutant claim is made.

After resolving OPEN-ENV-01, retain this branch and complete only HP-5.
Append candidate commits without rewriting pushed history, then append a
new separate final delivery report.

Proposed CI additions after completion, not applied:
`python -B tests/compact_ann_oracle.py --self-test` and
`python -O -B tests/compact_ann_oracle.py --self-test`;
register `compact_ann_oracle` alongside the existing compact oracles in
`tests/ci_workflows.py`.

## Evidence

- `git status --short`: empty before setup.
- `git fetch origin`: exit 0.
- `git switch mahpate-pool-briefs-w4`: exit 0.
- `git pull --ff-only`: exit 0, already up to date.
- `git switch -c mahpate-hp5-p3c-ann-oracle 136d72c5a94e1c0d1878f99c461a555a91b7c3f6`:
  exit 0.
- `git --version`: exit 0, Git 2.56.0.windows.4.
- `python --version`: exit 0, Python 3.13.17.
- `rustc --version`: PowerShell command-not-found, containing command exit 1.
- `cargo --version`: PowerShell command-not-found, exit 1.
- Oracle and regression commands: not run, environment stop condition.
- Only added file: this status report, 63 lines.
