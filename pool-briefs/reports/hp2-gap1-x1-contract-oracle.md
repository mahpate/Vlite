# HP-2 delivery: X1 contract and independent oracle

Branch: `mahpate-hp2-gap1-x1-contract-oracle`.
Candidate: `61f167c97d669b77cb9e64bddeadb2d6d0f2177a`.
Base: `136d72c5a94e1c0d1878f99c461a555a91b7c3f6`.
The candidate is the last commit before this separate, non-integrable report.

## Candidate

New files only:

| File | Lines |
|---|---:|
| `docs/projects/roadmap/gap1-x1-logical-delete-contract.md` | 280 |
| `tests/memory_x1_oracle.py` | 779 |
| `tests/fixtures/agent_memory/x1_golden.json` | 453 |

The contract remains **PROPOSED**. No ruling is represented as approved.
The oracle models mixed dispositions, strict v2 bytes/receipts, evidence ownership
and transitive sharing, incident/content cleanup, dry-run nonmutation and commit
revalidation, retained versus intentionally deleted provenance, replay, expiry,
preappend refusal, guard budgets and expired receipt suffix cleanup.

Ratify especially: separate `consolidate-x1` operation and v2 domains; hashed
source manifest in at most six 4,096-byte properties; C1-only S1 rejecting X1
until separately extended; dry-run ignoring receipts and reporting the first
state conflict with unavailable estimates null; binary/hex outcome chunking;
and actual-revision replacement/deletion guard accounting. The existing receipt
key helper's `:v1` envelope is deliberately unchanged.

Literal binary/hash witnesses were independently assembled from the proposal,
not produced by Rust or captured from oracle output. Smallest-delete arithmetic:
4+912+16+618+527=2,077 body bytes. Coverage includes 64/65 and 256/257 inputs,
4,095/4,096/4,097 logical operations, six-property manifest packing, a 66-chunk
outcome maximum and 33-to-1 expired receipt replacement.

**Mutations:** 11 killed at their named golden scenarios, restored pass after
each; zero survivors and zero equivalents. These are oracle mutations, not
qualification of the future Rust implementation. Self-test prints witnesses.

**Not done:** Rust X1, actual four-backend reopen, I/O recovery, S1/E1 integration,
public surfaces, CI edits or release qualification. JSON reopen is logical only.
No accepted implementation/oracle/fixture, other package or shared file changed.

**Coordination/deviations:** owner confirmed this machine owns HP-2 while the
other three cover HP-1/3/4. No additional work was dispatched. Handover reread at
`a0f42e88` adds only HP-1 clarification. The shared checkout's inherited
autocrlf configuration was restored; only new files were explicitly LF-formatted.
No unresolved byte ambiguity was silently assigned; all new X1 choices are in
the proposed ruling table.

## Evidence (exact commands; all final checks exit 0)

- `python -B tests\memory_x1_oracle.py --self-test` and `python -O -B tests\memory_x1_oracle.py --self-test`: each 54 goldens, 11/11 mutations; 10 malformed receipts and seven malformed outcomes rejected.
- `python -B tests\agent_memory_oracle.py --self-test` and `python -O -B tests\agent_memory_oracle.py --self-test`: each 96 goldens, 40/40 mutations.
- `python -B tests\memory_ranking_oracle.py --self-test` and `python -O -B tests\memory_ranking_oracle.py --self-test`: each 42 ranking cases, 25/25 mutations.
- `python -B tests\c1_consolidation_oracle.py --self-test` and `python -O -B tests\c1_consolidation_oracle.py --self-test`: each 153 goldens, 3/3 mutations.
- `python -B tests\release_docs.py`: 14 pages/READMEs; `python -B tests\agent_docs.py`: nine tools/four skills; `python -B tests\content_budget.py`: nine budgets.
- `git diff --cached --check`; `git diff --check`; `git diff --name-status 136d72c5a94e1c0d1878f99c461a555a91b7c3f6..HEAD`: candidate has exactly three A lines.
- Python 3.13.17, Git 2.56.0.windows.4. Optional Rust probe was unavailable (exit 1); HP-2 needs no Rust build. No tool was installed.

## Proposed coordinator-only CI changes

Add these lines to the existing oracle block in `.github/workflows/ci.yml`:

```yaml
python tests/memory_x1_oracle.py --self-test
python -O tests/memory_x1_oracle.py --self-test
```

Add `"memory_x1_oracle",` to the tuple in
`tests/ci_workflows.py::Policy::test_graph_oracles_run_before_ci_routing`.
Do not integrate this report commit.
