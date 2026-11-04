# HP-5 partial delivery: OPEN-ANN-01

Status: WIP, stopped under section 5.6's unspecified-byte rule; not ready
for integration or Rust codec implementation.

- Branch: `mahpate-hp5-p3c-ann-oracle`.
- Base: `136d72c5a94e1c0d1878f99c461a555a91b7c3f6`.
- Partial candidate: `d9ba0338774ddfe34dac3d7a0ca0521cbef6ff18`.
- Candidate is that single new commit only; exclude earlier status-report
  commit `c7e774a539ba196b62f7e055018f490d33aa0c61` and this final report commit.

| Candidate addition | Lines |
|---|---:|
| `tests/compact_ann_oracle.py` | 1164 |
| `tests/compact_ann_report.md` | 75 |
| `tests/fixtures/compact/ann-v1/goldens.json` | 90 |
| `tests/fixtures/compact/ann-v1/malformed.json` | 129 |
| `tests/fixtures/compact/ann-v1/manifest.json` | 29 |
| `tests/fixtures/compact/ann-v1/README.md` | 59 |

OPEN-ENV-01 is resolved by handover `d8926ea4`; no history was rewritten.
The later user-directed Rust/Visual C++ development-tool installation was
separate from HP-5. No Rust generated or validated oracle expectations.

**OPEN-ANN-01:** C.3 persists scalar f64 minima and requires existing
training semantics; C requires deterministic bytes. Existing `f64::min`
permits either signed zero on an equal-zero tie. The detailed report cites
the repository and official Rust documentation. Python first-equal minimum
would silently freeze an additional byte rule. The reference returns `open`
for ambiguous minima rather than choosing bytes or calling valid source
corrupt. Proposed ruling: canonicalize persisted zero minima to +0 on
encode/bind, without changing C or public ScalarU8. Not adopted.

The coherent portion has ten hand-assembled body ledgers, literal B/G/C/set
digests, partition arithmetic, scalar/level rules, conditional rotated
witness checks, logical atomic binding and 123 malformed definitions tested
under both precisions. Nineteen named mutants were killed; zero survivors,
zero equivalents; every named scenario passed again after restoration.
The endian mutant simulates an opposite-endian host.

Remaining: resolve the byte rule, add its goldens, finish the explicit
coverage inventory and missing boundary witnesses, and complete independent
tampered-expected-golden checks. Outer source validity/authentication are
caller preconditions; there is no real image reader or backend qualification.
Rotation computation and lifecycle remain excluded. Nothing is activated.

After ratification, retain this branch, append fixes and a new final report;
do not treat green partial self-tests as full acceptance.

Proposed future CI lines, not applied:
`python -B tests/compact_ann_oracle.py --self-test` and
`python -O -B tests/compact_ann_oracle.py --self-test`;
register `compact_ann_oracle` in `tests/ci_workflows.py`.

## Evidence

- `git fetch origin`; `git switch mahpate-pool-briefs-w4`;
  `git pull --ff-only`; `git switch mahpate-hp5-p3c-ann-oracle`: all exit 0.
  Re-read amended section 7 and 22:20 update before resuming.
- `python -B tests\compact_ann_oracle.py --self-test`: exit 0.
- `python -O -B tests\compact_ann_oracle.py --self-test`: exit 0.
  Each reports 275 named checks, ten body ledgers, 123 malformed definitions
  per precision, 19 killed mutants, and explicit partial/OPEN status.
- Initial development run of the normal command exited 1 (`settings length`):
  corrected the hand ledger from 33 settings bytes to the prescribed 32;
  subsequent normal/optimized runs passed.
- All 16 regression invocations in this exact PowerShell loop exited 0:

  ```powershell
  $oracles = @('tests\compact_float16_oracle.py') + @(Get-ChildItem tests\compact_graph*_oracle.py | ForEach-Object { $_.FullName })
  foreach ($oracle in $oracles) {
      python -B $oracle --self-test
      if ($LASTEXITCODE -ne 0) { throw "Normal regression failed: $oracle" }
      python -O -B $oracle --self-test
      if ($LASTEXITCODE -ne 0) { throw "Optimized regression failed: $oracle" }
  }
  ```

- Regression coverage: float16 (80 goldens); graph attachment (7), import
  (19), label (4), CSR (15), overlay (5), property (3), schema (14).
- `git diff --cached --check`: exit 0 before candidate commit.
  Explicit formatting verified LF/no CR in all six additions.
- `git status --short`: empty after candidate commit; temporary regression
  fixture directories were removed by their owning tests.
