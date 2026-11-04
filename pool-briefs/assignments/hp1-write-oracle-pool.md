# HP-1 resume: Gap 6 write oracle (independent Python reference)

You continue work package HP-1 of the VLite project: an independent, Python-only reference oracle for the Gap 6
(`vlite-cypher-1` query subset) WRITE path: CREATE, MERGE, SET, REMOVE, DELETE and DETACH DELETE with their schema,
time, limit and atomicity rules. An earlier machine stopped before writing any oracle code because one byte rule
(OPEN-W01) was unresolved. The coordinator has resolved it (ruling R-HP1-01, below). Your checkout already holds that
machine's stop reports and the complete package description.

## Where things are in your checkout

- `pool-briefs/handover-2026-11-04-parallel-pickup.md`: read sections 0 to 4, section 5.1 (THE PACKAGE: why, read
  first, independence rule, deliverables, coverage, mutation campaign, done-when, stop conditions), section 7 and
  the Coordinator updates (the 21:45 PT ruling R-HP1-01). The git and delivery instructions in that document are for
  people working on their own branch. In this job the JOB RULES below override them: you do not create branches, you
  do not commit and you do not push.
- `tests/graph_subset_write_report.md` (the earlier machine's report, including OPEN-W01),
  `pool-briefs/reports/hp1-gap6-write-oracle.md` and `pool-briefs/reports/hp1-gap6-write-oracle-status.md`.
- `tests/graph_subset_oracle.py` and `tests/fixtures/graph_subset/` (the accepted read oracle and fixtures: import
  from the oracle, never edit it; mirror the fixture conventions).
- `docs/projects/roadmap/gap6-cypher-subset.md` (the design: sections 3 to 7, 9, 11 and 12).

## Job rules (this is a worker-machine task)

- Edit files in your checkout only. Do NOT run `git add`, `commit`, `push`, `switch`, `checkout`, `reset` or `stash`:
  your uncommitted changes are collected for you. Do not write outside the checkout except short-lived temp files
  that you delete.
- Python only (3.11-compatible code). No Rust, no cargo. NEVER run or transliterate Rust. Expected values are
  hand-authored from the design and the documented format; never taken from running the code under test.
- Write only these paths: `tests/graph_subset_write_oracle.py`, `tests/fixtures/graph_subset/write-cases.json`,
  `tests/fixtures/graph_subset/write-mutant-map.json`, `tests/fixtures/graph_subset/WRITE-README.md`,
  `tests/graph_subset_write_report.md` (rewrite it), and `pool-briefs/reports/hp1-gap6-write-oracle.md` (the final
  report; replace the WIP text). Do not edit any other file, including this one and the handover document.
- Independence: do not read or copy any Rust under `src/property_graph/engine/subset/` or
  `src/property_graph/subset.rs`, nor any other Gap 6 branch or candidate.
- If a design rule contradicts another section or an approved decision, record an OPEN item instead of choosing.

## Ruling R-HP1-01 (resolves OPEN-W01; authoritative)

The op-12 body byte grammar is the existing storage format, not the Gap 6 subset implementation, so it is authorized
ground truth for the write oracle. Extra reading is allowed for FORMAT FACTS only:
`docs/projects/feature-agent-graph/decisions.md` (the DO-003 body header and entry table, and "Gap 3 graph frame rule"
for reclaim entries and the prefix `id` and `payload_len` bounds), `docs/GRAPH.md`,
`docs/projects/roadmap/gap1-c1-consolidation-contract.md` lines 25 and 322 (a worked, independently validated size
precedent), `src/property_graph/engine/codec.rs` and `engine/codec/*.rs` (layouts, constants and size arithmetic
only), `engine/properties.rs` (property encoding), `engine/mutate.rs` (only to see which entries each `GraphOp`
lowers to and when reclaim entries are added) and `src/property_graph/frame.rs` (only the op-12 prefix rule).
Still forbidden: `src/property_graph/subset*`, `src/property_graph/engine/subset/**` and every other Gap 6 branch.
Safeguards: derive each entry size by hand from the documented table first, cite file and line for every constant,
then cross-check against `codec.rs`; a mismatch between the document and the codec is a new OPEN item for the
coordinator, not a choice. Expected byte counts stay hand-authored, with the arithmetic in comments. Rewrite
OPEN-W01 in `tests/graph_subset_write_report.md` as RESOLVED with the pinned byte contract, then complete section 5.1
in full, including `body-at-cap`, `body-over-cap`, their reclaim-inclusive counterparts and the omit-overhead mutant.

## Order of work (keep the self-tests green after every milestone)

Your changes are collected only when you finish, so keep the checkout runnable at all times and finish a milestone
before starting the next.

1. Infrastructure and the pinned byte contract (OPEN-W01 RESOLVED), then CREATE, MERGE, SET and REMOVE scenarios
   with the self-test passing. Write a first version of the report now.
2. DELETE and DETACH DELETE, binding semantics (materialized MATCH, empty MATCH, deleted binding errors).
3. Schema constraints, time (captured clock once per statement; C-1, C, C+1 expiry), then the limits and exact
   byte arithmetic (4,095, 4,096, 4,097 operations; body exactly at and one over the cap, with and without reclaims).
4. The hand-built mutation campaign: every mutant needs a named failing scenario (the list is in section 5.1).
5. Final report with counts, files, OPEN items and the two proposed CI lines. Target 150 or more scenarios.

If the new content would exceed about 4,500 lines, stop after the first coherent portion (milestones 1 and 2), make
sure its self-test passes, and propose the split in the report.

## Done when (all must hold)

```powershell
python -B tests/graph_subset_write_oracle.py --self-test
python -O -B tests/graph_subset_write_oracle.py --self-test
python -B tests/graph_subset_oracle.py --self-test
python -O -B tests/graph_subset_oracle.py --self-test
```

all exit 0; `git diff --check` is clean; the files you changed contain no CR characters; the report proposes the CI
lines (`python -B tests/graph_subset_write_oracle.py --self-test` and the same with `python -O -B`) and the name for
`tests/ci_workflows.py` without applying them. Your final chat message must be at most 150 words: scenario count,
mutants killed and survived, files changed, open items.
