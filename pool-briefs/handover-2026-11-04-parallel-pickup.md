# Parallel pickup handover (2026-11-04)

Audience: a Copilot session on a worker machine with a fresh clone of
`https://github.com/mahpate/Vlite.git`. You see only this repository and the prompt you were
given, so everything you need is here or in the repository.

| Item | Value |
|---|---|
| This document | branch `mahpate-pool-briefs-w4`, file `pool-briefs/handover-2026-11-04-parallel-pickup.md`. Never merge this branch or link it from any other file: it is not public documentation |
| Code base for every package | commit `136d72c5a94e1c0d1878f99c461a555a91b7c3f6` |
| Released version | `v0.6.1` (tag). Nothing described below is released, tagged or enabled by default |
| Coordinator | the owner's primary PC. Only the coordinator integrates work, edits status docs and CI, and decides open questions |
| Your role | one package, your own branch, new files only, one report |

## 0. What you do (read this first)

1. Do exactly ONE package: the one named in your prompt (HP-1 to HP-4, or a verification request
   as in section 5.5). Never start a second one, even if you finish early. Report and stop; the
   owner assigns the next one.
2. Work on your own branch `mahpate-hp<N>-<slug>`, created from the code base commit above.
3. Your changes are NEW files inside the paths your package lists. Editing an existing file is out
   of scope unless the package says so. If you believe one must change, stop and say why.
4. Validate with the commands in your package. Finish with the report commit (section 4) and push
   only your own branch.
5. The coordinator re-validates your work in a clean checkout, may rebuild any mutant, and
   integrates by cherry-pick. Nothing you push is merged automatically.
6. If this document and your prompt disagree, follow the stricter rule and note the conflict in
   your report.

## 1. The project in brief

- VLite is an embedded vector database written in Rust (crate `vlite`): single-file and directory
  stores, exact, ANN and hybrid search, metadata filters, TTL, a REST/gRPC/MCP server, an offline
  CLI and language bindings. The latest release is `v0.6.1`.
- Since `v0.6.1` the integration branch `mahpate-graph-store-continuation` has gained about 276
  commits (about +152k lines) of storage and AI-feature work: compact vectors (P3b), compact graph
  (P4), filter-aware ANN (P5), property graph and TTL (Gaps 3 and 5), key-value (Gap 2), temporal
  graph (Gap 4), agent memory (Gap 1), Cypher-style query subset (Gap 6), GraphRAG ingestion
  (Gap 7) and serving surfaces (Gaps 8 and 12).
- Safety rule of the whole plan: every new capability is opt-in per file or collection,
  crate-private, or inactive. The default path must behave exactly as `v0.6.1`, and the owner
  approves each public surface (Rust names, wire shapes, errors) before it is exposed.
- Release plan: the owner decided to keep the full 0.6.2 plan as is (17 release gates). The
  authoritative roadmap is `docs/projects/roadmap/next-major.md`. Design documents live beside it
  in `docs/projects/roadmap/`.
- Method: work moves in small slices. An implementer session writes the code. An independent
  session writes the contract and the oracle (an executable reference whose expected values are
  authored by hand from the design, never produced by the code under test) first. The coordinator
  then accepts each slice with hand-built mutants ("fail-first": a deliberately broken variant
  must make the tests fail for the intended reason and pass again when restored) and integrates
  with `git cherry-pick -x`.

## 2. Snapshot (2026-11-04, evening Pacific time)

The pushed integration head is `1acd3ba041b8e75e1388fcde701299b3899b10cb` (CI green, 24 of 24
jobs). The code base for your packages adds two documentation commits (the Gap 1 slice C scoping
and its acceptance). Nothing is released or tagged.

### 2.1 In flight on the coordinator PC: do not touch these paths

| Lane | Paths it owns |
|---|---|
| P3b slice 3 (ordered reads over base plus overlay) | `src/records.rs`, `src/collection.rs`, `src/lib.rs` search paths, `src/hybrid.rs`, `src/query_language.rs` |
| Gap 8 step 2b (gRPC provider contract) | `proto/vlite/v1/vlite.proto`, `src/server/grpc.rs`, `src/server/features.rs`, `docs/GRPC.md`, one row of `site/api.html` |
| Gap 6 read executor | `src/property_graph/engine/subset/**`, `src/property_graph/subset.rs`, narrow hooks in `src/property_graph/read.rs` and `temporal/read.rs`, `tests/graph_subset_read.rs` |
| Gap 7 canonical schema v1 | new `docs/projects/roadmap/gap7-canonical-schema-v1.md`, `tests/ingestion_schema_reference.py`, `tests/fixtures/ingestion/schema-v1/**` |
| Gap 1 S1 (read-only stale-summary status) | new `src/memory/stale.rs` and `src/memory/stale/tests.rs`, one `mod` line in `src/memory.rs` |
| Gap 1 E1 contract and oracle (erasure barrier) | new `docs/projects/roadmap/gap1-e1-erasure-barrier-contract.md`, `tests/memory_erasure_oracle.py`, `tests/fixtures/agent_memory/erasure_golden.json` |

### 2.2 Held or blocked: do not start

- Key-value serving and the KV lease ledger (owner decisions pending).
- Gap 4 slice D and the Gap 1 fact slices F1 to F6 (owner decisions pending).
- Gap 7 core (waits for the schema above) and Gap 7 providers.
- P4 restart; Gap 6 write executor and later Gap 6 serving (wait for the read executor).
- Any other serving lane: serving work runs one lane at a time because every serving change
  touches the same wire files.

### 2.3 Owned by another team: never touch

`src/dashboard/**`, `src/server/admin.rs` and the hosted-CI workflows.

## 3. Working agreements (non-negotiable)

1. **Own branch only.** Create `mahpate-hp<N>-<slug>` from the code base commit. Never push to any
   other branch (in particular `main`, `mahpate-graph-store-continuation` and every
   `mahpate-pool-briefs*` branch). No force-push, no tags, no pull requests, no issues, no
   comments, no deleting remote branches.
2. **New files only**, inside your package's allowed paths. The coordinator owns existing files,
   `.github/workflows/**`, `Cargo.toml`, `Cargo.lock`, `site/**`, `README.md` and every status
   document. If a CI line or doc change is needed, propose its exact text in your report.
3. **No public mention.** Never mention or link the worker pool, this document or `pool-briefs/`
   from any file outside `pool-briefs/`.
4. **Independence.** An oracle or contract author never reads the implementation that will later be
   judged against it, and an implementer never edits an oracle. Expected values are authored from
   the design, not by running the code under test.
5. **Evidence discipline.** Record exact commands and their exit codes. A claim without a command
   is not evidence. No performance, size or timing claims, and no benchmarks.
6. **Never guess bytes or semantics.** If the repository does not settle a byte-level or semantic
   rule, record an OPEN item (ruling table or report) with your proposed ruling and rationale.
   Do not choose silently.
7. **Python.** Standard library only; deterministic (no wall clock, no unseeded randomness); must
   also pass under `python -O` (do not rely on `assert` for checks); JSON written with
   `allow_nan=False`; LF line endings (run `git config core.autocrlf false` in your clone);
   `git diff --check` must be clean.
8. **Rust builds** (only where a package needs them): `CARGO_INCREMENTAL=0`,
   `CARGO_BUILD_JOBS` at most half the logical CPUs, a package-local `target` directory,
   `--locked`, one cargo process at a time. Add `--offline` only when the registry cache already
   holds the crates.
9. **Commits.** Small and logical, imperative messages, each ending with the trailer
   `Co-authored-by: Copilot App <223556219+Copilot@users.noreply.github.com>`. Do not rewrite
   history after you push; add fix-up commits instead.
10. **Secrets.** None are needed. Never paste or commit tokens, machine paths or logs that contain
    them.
11. **Blocked or out of context.** Commit your work with a `WIP:` prefix, push your branch, write
    `pool-briefs/reports/hp<N>-<slug>-status.md` saying exactly what is done, what remains and how
    to resume, then stop. The coordinator may hand the rest to another machine.

## 4. Delivery protocol

Setup:

```powershell
git clone https://github.com/mahpate/Vlite.git vlite-hp
cd vlite-hp
git config core.autocrlf false
git switch mahpate-pool-briefs-w4        # read this document from here
git switch -c mahpate-hp<N>-<slug> 136d72c5a94e1c0d1878f99c461a555a91b7c3f6
```

Work, validate, commit. Then add the report as a separate, final commit and push:

```powershell
git push origin mahpate-hp<N>-<slug>
```

Tell your operator the branch name, the final SHA and the candidate SHA (the last commit before
the report). The coordinator polls `origin` for `mahpate-hp*` branches, so no other notification
is needed.

**Report** (`pool-briefs/reports/hp<N>-<slug>.md`, at most 400 words plus an evidence list):

- candidate SHA, base SHA, files added with line counts;
- what is modeled or frozen, and what is explicitly not;
- the ruling-table decisions or OPEN items that most need the coordinator's ratification;
- mutation counts: killed, survived, equivalent (with the argument for each equivalent);
- validation commands with exit codes and counts;
- deviations from this document, and any conflict you found;
- exact CI lines or document edits you propose (do not apply them);
- anything not done.

**Acceptance (informational).** The coordinator re-runs your validation in a clean checkout,
re-builds mutants, reviews the whole diff, then cherry-picks the candidate commits with `-x`. The
report commit is never integrated.

## 5. Work packages

Relative size compares packages with each other only. The four packages touch disjoint new paths,
so machines working in parallel cannot conflict.

| Package | Branch | Size | Paths you may add |
|---|---|---|---|
| HP-1 Gap 6 write oracle | `mahpate-hp1-gap6-write-oracle` | L | `tests/graph_subset_write_oracle.py`, `tests/fixtures/graph_subset/write-*`, `tests/graph_subset_write_report.md` |
| HP-2 Gap 1 X1 contract and oracle | `mahpate-hp2-gap1-x1-contract-oracle` | L | `docs/projects/roadmap/gap1-x1-logical-delete-contract.md`, `tests/memory_x1_oracle.py`, `tests/fixtures/agent_memory/x1_golden.json` |
| HP-3 Cross-version check | `mahpate-hp3-cross-version-check` | L | `scripts/cross_version_check.py`, `scripts/cross_version_probe.rs`, `tests/fixtures/cross_version/**` |
| HP-4 Default-path audit | `mahpate-hp4-default-path-audit` | M | report only |
| HP-5 P3c ANN codec oracle | `mahpate-hp5-p3c-ann-oracle` | L | `tests/compact_ann_oracle.py`, `tests/fixtures/compact/ann-v1/**`, `tests/compact_ann_report.md` |

Every package may also add its own report under `pool-briefs/reports/`.

### 5.1 HP-1: Gap 6 write oracle (independent Python reference)

**Why.** The `vlite-cypher-1` query subset (design: `docs/projects/roadmap/gap6-cypher-subset.md`)
has an accepted parser and an independent read oracle. The read executor is being implemented now.
The write executor follows it and must be accepted by replaying an independent write oracle, but
`tests/graph_subset_oracle.py` returns `NOT_MODELED` for every mutating statement. Authoring the
write oracle now removes a wait later.

**Read first, in this order.**

- `docs/projects/roadmap/gap6-cypher-subset.md`: sections 3 (language), 4 (model and
  expressions), 5 (writes, schema, time, atomicity), 6 (limits and accounting), 7 (errors),
  9 (independent oracle and qualification), 11 and 12 (approved decisions and provisional
  diagnostics).
- `tests/fixtures/graph_subset/README.md`: container and hash rules, case schema, groups, mutation
  switches, what is `NOT_MODELED` or open. Mirror these conventions.
- `tests/graph_subset_oracle.py` (accepted; import from it, never edit it) for the parser, graph
  model, value ordering and the `evaluate_read` shape.
- Ground truth for batch semantics: `src/property_graph/mod.rs` (`GraphOp`, `Expected`),
  `src/property_graph/engine/limits.rs` (4,096 entries, 16,777,216 body bytes, 256-byte keys, 64
  properties, 65,536 property bytes) and `src/property_graph/engine/state.rs` (incident-edge
  enumeration used by DETACH).

**Independence rule.** Do not read or copy any Rust under `src/property_graph/engine/subset/` or
`src/property_graph/subset.rs`. Expected values are hand-authored from the design.

**Deliverables (new files only).**

1. `tests/graph_subset_write_oracle.py`: stdlib only, `--self-test` (also under `python -O`),
   importing the accepted module. `evaluate_write(graph, statement, params=None, limits=None,
   clock=None)` returns a tagged result in the same style as `evaluate_read` and adds: the ordered
   staged operations as plain dicts, the commit-frame count (0 or 1), counters, and the complete
   graph after the statement (nodes, edges, IDs, revisions, expiry, attachments). On any failure
   the result carries the entire original graph, unchanged (copy-on-write transaction).
2. `tests/fixtures/graph_subset/write-cases.json`, `write-mutant-map.json` and `WRITE-README.md`,
   with new schema names (`vlite.graph-subset-write`, `vlite.graph-subset-write-mutants`) and the
   same content-hash rule as the read fixtures.
3. `tests/graph_subset_write_report.md`: the OPEN questions you found, each with a citation, the
   conservative interpretation you pinned and the case that pins it.
4. A hand-built mutation campaign (see below).

**Coverage (hand-authored original scenarios; target 150 or more).**

- CREATE node and edge: live-key conflict; keyed edge create is idempotent for identical
  endpoints, type and bit-identical properties and conflicts otherwise; keyless edges always
  allocate; per-row creation; row-local new bindings.
- MERGE: match-or-create on an explicit key only (never by property scan or endpoint triple);
  required labels and equal properties, extras remain; an incompatible occupied key is
  `GraphKeyConflict`; later rows see tentative state.
- SET and REMOVE: only named properties or labels; removal of an absent member is a no-op;
  keys, IDs, endpoints and relationship types are immutable; patches retain expiry; one operation
  per clause and entity, never combined across clauses.
- DELETE and DETACH DELETE: edges in ascending ID order before nodes in ascending ID order;
  `GraphNodeHasEdges` for any retained incident edge including self-loops and edges not valid at
  the captured time; DETACH enumerates incident edges lazily, charges each to the work meter and
  fails `BatchTooLarge` as soon as staged operations would exceed 4,096; attachments are dropped
  with the node and vector records are kept.
- Binding semantics: MATCH is materialized once; an empty MATCH applies nothing; no MATCH means
  one empty row; clauses run in textual order over canonical row order; reading a deleted binding
  (including in RETURN) fails with `InvalidQuery("subset binding was deleted")`.
- Schema: required, type and unique properties, endpoint labels and UniqueTriple; MERGE never
  bypasses them; a failure on the final operation rolls everything back.
- Time: the captured clock is sampled once per statement, never per clause; expired-key reuse;
  node and edge expiry at C-1, C and C+1; closed temporal incident edges.
- Limits: 4,095, 4,096 and 4,097 operations; body bytes exactly at and one over the 16 MiB cap
  including encoding overhead; the commit-frame count (exactly one, or none when nothing changes).
  The write deadline and scratch values are validated as inputs but execution cost, hold time and
  cancellation stay `NOT_MODELED`, as in the read oracle.

**Mutation campaign.** Every mutant needs a named failing scenario. At least: MERGE by endpoint
triple; bypass of unique, required and endpoint constraints; omit one DETACH edge; off-by-one TTL
or validity interval; sample the clock per clause; accept 4,097 operations; omit encoding
overhead; append before final validation; retain partial cleanup; reverse delete order; combine
patches across clauses; treat a no-op REMOVE as an error.

**Done when.**

- `python -B tests/graph_subset_write_oracle.py --self-test` and the same under `python -O -B`
  exit 0.
- `python -B tests/graph_subset_oracle.py --self-test` and the same under `-O` still exit 0.
- `git diff --check` is clean, and `git diff --name-status 136d72c5a94e1c0d1878f99c461a555a91b7c3f6..HEAD`
  shows only `A` lines.
- The report proposes the two CI lines (self-test and `-O`) for the oracle list in
  `.github/workflows/ci.yml` and the name for `tests/ci_workflows.py`.

**Stop and report** if a section 5 rule contradicts another section or an approved decision (record
an OPEN item instead of choosing), or if the new content would exceed about 4,500 lines (commit the
first coherent portion: CREATE, MERGE, SET, REMOVE; propose the split).

### 5.2 HP-2: Gap 1 X1 consolidation `logical_delete` (frozen contract plus oracle)

**Why.** The memory serving contract offers `disposition` of `retain` or `logical_delete` for every
consolidation input (`docs/projects/roadmap/serving-memory-contract.md:338-339` and `:675`), so
agent-memory serving cannot be complete without it. The owner kept this slice in the plan
(`docs/projects/roadmap/gap1-c2-remainder-contract.md`, section 9, question 3). The accepted C1
consolidation implements only the retain disposition. This package writes the new frozen contract
and the independent oracle that the later Rust slice must reproduce.

**Read first.**

- `docs/projects/roadmap/gap1-c2-remainder-contract.md`: sections 1 and 2 (base facts), 5 (shared
  rules), 6 (sub-slices X1 and S1), 7 (decisions 2, 3 and 13) and 9 (rulings).
- `docs/projects/roadmap/gap1-c1-consolidation-contract.md` (the frozen C1 contract: your
  structure template and the bytes you must not change) and `docs/projects/roadmap/gap1-agent-memory.md`
  sections 4 and 5.
- `docs/projects/roadmap/serving-memory-contract.md` (consolidation and forget sections).
- Ground truth, read-only: `src/memory/consolidate.rs` and `src/memory.rs` (the A2 forget planning:
  detach, shared-evidence and foreign-attachment rules).
- Oracle templates: `tests/c1_consolidation_oracle.py` with
  `tests/fixtures/agent_memory/c1_consolidation_golden.json`, and `tests/agent_memory_oracle.py`.

**Deliverables (new files only).**

1. `docs/projects/roadmap/gap1-x1-logical-delete-contract.md` (status PROPOSED; the coordinator
   ratifies). Mirror the C1 contract and freeze, byte-exactly wherever something is persisted or
   digested:
   - request and response types: the per-input disposition and the dry-run plan (conflicts and
     operation and byte estimates, not a reservation), with validation and limits (1 to 256
     inputs; 4,096 operations and 16 MiB per frame);
   - `logical_delete` semantics using the A2 forget rules (detach authorized evidence, delete only
     unshared records, retain and report shared evidence, a foreign attachment blocks deletion
     with an explicit error);
   - summary provenance when a source is deleted by the same consolidation. The C1 `DERIVED_FROM`
     edge is removed with its source, so a read-only status that counts retained edges would
     misreport the summary as stale. Define the persisted record that avoids this (options table,
     choice, rationale), or state exactly how the S1 stale rule must treat deleted sources;
   - receipts and digests v2: a new operation name and version and a new domain string, with the C1
     v1 bytes unchanged;
   - atomicity (summary, receipt and deletions in one frame, atomic refusal), retry and replay,
     reopen on plain and encrypted file and directory stores;
   - a ruling table (option, choice, rationale) for every open point, and a "not frozen" list.
2. `tests/memory_x1_oracle.py` (`--self-test`, also under `python -O`) and
   `tests/fixtures/agent_memory/x1_golden.json` (additive; the C1 golden is untouched).
3. A hand-built mutation campaign over the oracle: kills, survivors and an equivalence argument for
   each survivor.

**Concurrent work you must not depend on.** The erasure-barrier contract (E1) is being written by
another session and the read-only stale-summary status (S1) is being implemented. Assume nothing
about them beyond the C2 document; write every assumption into the ruling table.

**Done when.**

- The new oracle's self-test (and under `-O`) exits 0, and the existing three memory oracles'
  self-tests (`agent_memory_oracle`, `memory_ranking_oracle`, `c1_consolidation_oracle`, each also
  under `-O`) still exit 0.
- `python tests/release_docs.py`, `python tests/agent_docs.py` and `python tests/content_budget.py`
  exit 0, `git diff --check` is clean, and only `A` lines appear against the code base commit.
- The report names the ruling-table choices that most need ratification.

**Stop and report** if the contract conflicts with Gap 1 sections 4 and 5 or an approved decision,
if a byte rule cannot be settled from the repository, or if new content would exceed about 2,500
lines (commit the first portion and propose a split).

### 5.3 HP-3: cross-version compatibility and behavior check (evidence)

**Why.** Everything new is meant to be invisible on the default path. The repository has a
one-off precedent for proving this at the file level (`scripts/graph_compat_check.py`, recorded in
`docs/projects/feature-agent-graph/decisions.md`, DO-001: 348 of 348 probes matched), but no
harness compares behavior across releases. This package produces one and its first result. It is
release-safety evidence, not a feature.

**Method (follow the DO-001 precedent).** Extract each revision with `git archive` into an isolated
directory, compile a throwaway probe crate against it into a per-revision target directory (no
global installs), run the same probe, and compare the outputs. Read `scripts/graph_compat_check.py`
and `scripts/graph_compat_probe.rs` first and reuse their conventions. Do not edit them.

**Revisions.** `v0.6.1` (reference) and `136d72c5a94e1c0d1878f99c461a555a91b7c3f6` (candidate),
both built with default features. Optionally also `v0.6.0`.

**Deliverables (new files only).**

1. `scripts/cross_version_check.py` (stdlib; subcommands `fixtures`, `run`, `compare`) and
   `scripts/cross_version_probe.rs` (compiled against each revision using only the `v0.6.1` public
   API so one source builds everywhere).
2. Small deterministic fixtures under `tests/fixtures/cross_version/` (fixed seeds, hex text for
   binary files, a SHA-256 manifest).
3. The report `pool-briefs/reports/hp3-cross-version-check.md` containing the matrices below.

**Matrix A, default-path behavior.** One deterministic workload on both revisions, with a
canonical transcript per revision (one JSON line per operation: arguments digest, result digest).
Cover: create, open, close and reopen; insert, upsert, delete, get; metadata update; exact search
with ties (every metric the library offers); filters; hybrid text-plus-vector search; browse and
pagination; batch writes; TTL expiry (with an injected clock if the API allows); maintenance and
compaction; backup and restore; the change feed; an encrypted file; a directory store. The
transcripts must be identical. Any difference is either listed with a justification or reported as
a finding. Do not fix findings; report them.

**Matrix B, file cross-open.** Files written by `v0.6.1` opened by the candidate (read everything,
then append) and files written or appended by the candidate in default mode opened by `v0.6.1`
(read everything). Expected: identical results, or a documented, explicit refusal.

**Matrix C, fail closed.** The candidate introduced header flags: graph (4), history (8), compact
(16), precision (32) and temporal (64), plus the format version constant
(`src/format.rs:18-101`). Construct synthetic flagged files byte-for-byte (as DO-001 did for the
graph flag), including encrypted and directory variants, and confirm `v0.6.1` refuses each with an
explicit error rather than misreading it, and that an unknown flag (for example 128) is refused by
the candidate and by `v0.6.1`.

**Done when.**

- `python scripts/cross_version_check.py run` produces both matrices from a clean checkout and
  exits non-zero on any unexplained difference.
- Both builds used `--locked`, per-revision target directories and no edits to the working tree.
- `python scripts/cross_version_check.py --help` documents every subcommand; `git diff --check`
  is clean; only `A` lines appear against the code base commit.
- The report shows the matrices, the toolchain versions, the exact commands with exit codes, and
  every difference with its justification or "finding" label.

**Stop and report** if a revision cannot be built on the machine, or if new content would exceed
about 2,000 lines.

### 5.4 HP-4: default-path regression audit (read-only review)

**Why.** Lines edited in files that run on the default path are the real regression risk of the
release. The coordinator's rough estimate is about 584 pre-existing lines in `src/**` (verify it and
report the true number and your method). Pure additions inside shared functions count too (for
example a new branch in a search routine).

**Task.** Compare `v0.6.1` with `136d72c5a94e1c0d1878f99c461a555a91b7c3f6` for `src/**`:

1. Isolate the hunks that modify or delete pre-existing lines, and the hunks that add a branch,
   argument or early return to a function the default path calls.
2. For each hunk record the file and line range, what changed, whether it is reachable with default
   features and no opt-in file flag, the tests or CI jobs that exercise it, a risk rating (none,
   low, medium, high) and the extra test you would add if the evidence is thin.
3. Group the table by module, put every medium or high item first, and state your confidence.

**Deliverable.** `pool-briefs/reports/hp4-default-path-audit.md` only. Do not add or change code or
tests; propose them in the report. Running `cargo test --locked` at the code base to confirm it
is green is welcome and optional.

**Done when** every hunk in the defined scope is in the table (or the report states precisely
what scope rule excluded it) and the report names the three items you would review first.

### 5.5 Verification requests (any machine, on demand)

The coordinator will sometimes hand you a candidate to verify instead of a package. The prompt
names a branch and a SHA. Then:

1. Make a fresh clone, `git switch --detach <sha>`, and confirm the parent commit and that only the
   listed files changed.
2. Run the validation commands the prompt lists (fmt, strict clippy with `-D warnings`, the
   relevant tests, documentation gates) and, if asked, hand-build mutants for the named behavior
   groups: break one behavior, run the exact selector (it must fail for the intended reason),
   restore, run it again (it must pass).
3. Change nothing in the candidate. Report only: commands, exit codes, counts, mutant outcomes,
   anything surprising. Push nothing except a report on a `mahpate-hp-verify-<name>` branch.

### 5.6 HP-5: P3c persisted ANN codec, independent Python oracle

**Why.** P3c stores a derived approximate-search index (flattened HNSW topology, scalar-u8 codes
and a rotated one-bit prototype) inside compact files. The design is owner-approved
(J.1 to J.9, 2026-11-02) but no code or byte oracle exists, and its section H requires byte goldens
to be committed and reviewed before any codec code. The Rust codec (sub-slice C1) cannot start
until the P3b source seam lands, so authoring the independent oracle now removes a wait later.
The package needs Python only.

**Read first, in this order.**

- `docs/projects/roadmap/p3c-compact-ann.md`: section A (scope), C.1 to C.5 (the codec), D (source
  binding), H (test plan), I (sub-slice C1) and J (approved decisions 1 to 9).
- `docs/projects/roadmap/compact-storage-contract.md`: the 32-byte envelope, section tables, frame
  prefix and unknown-kind skipping rules that the codec reuses unchanged.
- Existing code for facts the codec reuses (read only, never edit): `src/quantize.rs` (`ScalarU8`
  training, rounding and norm), `src/hnsw.rs` (the `Index::level(id)` rule with its seed and
  level-16 cap, and snapshot validation) and `src/lib.rs:95` (`MAX_DIMENSIONS`).
- Conventions of the independent compact oracles: `tests/compact_float16_oracle.py`,
  `tests/compact_graph_oracle.py` and `tests/fixtures/compact/*.json` (schema names, content-hash
  rule, mutation switches, `--self-test`, `python -O`). Mirror them.

**Independence rule.** No P3c Rust exists (`src/compact/ann.rs` is not written): never create or
read one. Expected bytes are hand-assembled from the document and confirmed by a separate hand
calculation, never produced by running Rust.

**Scope (sub-slice C1 only).** In: the envelope for kinds `0x0200` to `0x0203`; the descriptor
(binding 224 bytes, settings 32 bytes, parameters); topology (5 sections); scalar rows (3
sections); rotated rows as a byte layout (3 sections plus the 40-byte parameter block) whose row
contents are explicit input values (sign bytes and four f64 bit patterns), not computed; the
canonical greedy partition split; the digests B, G, C, the envelope digest and the index-set digest;
the rule that the 8-byte envelope digest is a guard and never a proof; the outcome classes
`ok`, `stale`, `corrupt`, `unsupported_version` and `unknown_kind`; atomic installation; and the
`ScalarU8` training, encoding and norm plus the level rule that `bind` needs. Out (later packages):
the rotation algorithm of section F (SplitMix streams, permutations, signs, pair rotations and
correction terms), lifecycle, rebuild, overlay, cache, encryption and threads.

**Deliverables (new files only).**

1. `tests/compact_ann_oracle.py`: stdlib only, with `--self-test` (also under `python -O`). Offer
   `encode_*`, `check_*`, `bind`, digest and `partition_plan` functions that return tagged results,
   in the style of the other compact oracles.
2. `tests/fixtures/compact/ann-v1/`: hex frames per kind for both precision codes (f32 = 1,
   f16 = 2), source-bit transcripts with B, G, C, envelope and index-set digests, partition-split
   cases (the 256-node and 4,096-row limits, the 1 MiB target with worst-case 63-byte padding per
   section, one row up to the 4 MiB cap), empty-index and N = 0 cases, the 768-dimension rotated
   sign payload of exactly 96 bytes, and a malformed-case manifest with one repairable defect per
   case and its expected outcome class. Use the same content-hash rule as the other fixtures.
3. `tests/compact_ann_report.md`: every OPEN question, each with a citation, the conservative
   reading you pinned and the case that pins it.
4. A hand-built mutation campaign (see below).

**Coverage (hand-authored; target 120 or more cases).**

- Every count, length and offset overflow and cap of C.5; duplicate, missing and out-of-order
  runs; out-of-range, wrong-level, self and duplicate neighbors; degree bounds (2m on level 0, m
  above); unsorted neighbor lists; level count 1 to 17; the entry-node rule and max-level
  mismatch; padding and trailing bytes; truncated image and end; NaN, negative scale, wrong norm,
  wrong minima or scales against independent retraining; constant dimensions (scale +0, code 0);
  empty training (+0 parameters); zero-norm cosine exclusion; signed zero in C; canonical f16 bits.
- Binding: a same-prefix, different-B simulation is not accepted; a foreign valid set is `stale`,
  not `corrupt`; an unsupported version skips the whole set atomically; an unknown kind is skipped;
  corruption at an understood boundary always beats a recoverable skip.
- Determinism: identical logical inputs at identical frame positions give identical bytes; section
  payloads and digests do not depend on padding or position.

**Mutation campaign.** Every mutant needs a named failing scenario. At least: wrong source-digest
bytes (16 to 23); omit a section from the index-set digest; reorder neighbors; accept neighbor
ordinal N; skip the level cap; accept degree 2m + 1 above level 0; round a code stride up; hash
padding into a digest; use native endianness; accept trailing bytes; treat stale as corrupt;
treat unsupported as corrupt; split a partition at 257 nodes; allow 64 bytes of section padding;
round half-even instead of half-away in scalar training; accept a NaN norm; omit one B section;
omit the lineage from G; drop the signed zero from C.

**Done when.**

- `python -B tests/compact_ann_oracle.py --self-test` and the same under `python -O -B` exit 0.
- The existing compact oracles still pass `--self-test` (`tests/compact_float16_oracle.py` and every
  `tests/compact_graph_*_oracle.py`).
- `git diff --check` is clean, and `git diff --name-status 136d72c5a94e1c0d1878f99c461a555a91b7c3f6..HEAD`
  shows only `A` lines.
- The report proposes the two CI lines (self-test and `-O`) and the name for `tests/ci_workflows.py`.

**Stop and report** if a rule in section C or D contradicts another section or the storage
contract, or a needed byte is unspecified (record an OPEN item instead of choosing), or if the new
content would exceed about 4,000 lines (commit the first coherent portion: descriptor, topology,
scalar rows and binding digests, then propose the split).

## 6. Paste-ready start prompts for the owner

Each prompt is complete on its own. Give each machine a different one.

HP-1:

```text
You are a parallel-pickup worker on the VLite project (https://github.com/mahpate/Vlite).
1. Clone the repository (or `git fetch origin`) and `git switch mahpate-pool-briefs-w4`.
2. Read pool-briefs/handover-2026-11-04-parallel-pickup.md completely; sections 0 to 4 and 7 apply to every package.
3. Execute ONLY work package HP-1 (Gap 6 write oracle) exactly as written there, on branch
   mahpate-hp1-gap6-write-oracle created from commit 136d72c5a94e1c0d1878f99c461a555a91b7c3f6.
4. Push only that branch, including your final report commit. No other branch, no pull request, no tag, no force-push.
5. Stop after reporting. Do not start another package.
```

HP-2:

```text
You are a parallel-pickup worker on the VLite project (https://github.com/mahpate/Vlite).
1. Clone the repository (or `git fetch origin`) and `git switch mahpate-pool-briefs-w4`.
2. Read pool-briefs/handover-2026-11-04-parallel-pickup.md completely; sections 0 to 4 and 7 apply to every package.
3. Execute ONLY work package HP-2 (Gap 1 X1 contract and oracle) exactly as written there, on branch
   mahpate-hp2-gap1-x1-contract-oracle created from commit 136d72c5a94e1c0d1878f99c461a555a91b7c3f6.
4. Push only that branch, including your final report commit. No other branch, no pull request, no tag, no force-push.
5. Stop after reporting. Do not start another package.
```

HP-3:

```text
You are a parallel-pickup worker on the VLite project (https://github.com/mahpate/Vlite).
1. Clone the repository (or `git fetch origin`) and `git switch mahpate-pool-briefs-w4`.
2. Read pool-briefs/handover-2026-11-04-parallel-pickup.md completely; sections 0 to 4 and 7 apply to every package.
3. Execute ONLY work package HP-3 (cross-version compatibility and behavior check) exactly as written there, on branch
   mahpate-hp3-cross-version-check created from commit 136d72c5a94e1c0d1878f99c461a555a91b7c3f6.
4. Push only that branch, including your final report commit. No other branch, no pull request, no tag, no force-push.
5. Stop after reporting. Do not start another package.
```

HP-4 (queue, for the first machine that finishes, or a fourth machine): the same prompt with
`HP-4 (default-path regression audit)` and branch `mahpate-hp4-default-path-audit`.

HP-5 (needs Python only; give it to any free machine, including one that finished another package):

```text
You are a parallel-pickup worker on the VLite project (https://github.com/mahpate/Vlite).
1. Clone the repository (or `git fetch origin`) and `git switch mahpate-pool-briefs-w4`.
2. Read pool-briefs/handover-2026-11-04-parallel-pickup.md completely; sections 0 to 4 and 7 apply to every package.
3. Execute ONLY work package HP-5 (P3c ANN codec independent oracle) exactly as written there, on branch
   mahpate-hp5-p3c-ann-oracle created from commit 136d72c5a94e1c0d1878f99c461a555a91b7c3f6.
4. Push only that branch, including your final report commit. No other branch, no pull request, no tag, no force-push.
5. Stop after reporting. Do not start another package.
```

## 7. Environment checklist (Windows)

- Tools: every package needs `git` and Python 3.10 or newer (CI uses 3.11). HP-1, HP-2 and HP-5
  need nothing else: a missing `rustc` or `cargo` is NOT a reason to stop for them, and you must
  not install or run Rust for them. HP-3, HP-4 and verification requests also need Rust 1.89 or
  newer (edition 2024) with `cargo`, `rustfmt` and `clippy`, and network access for crates (or a
  populated cache). No Docker or Node is needed.
- Check first: `git --version` and `python --version` (all packages); add `rustc --version` and
  `cargo --version` only for HP-3, HP-4 and verification requests. If a tool that your package
  needs is missing or too old, stop and report; do not work around it.
- Use a short working directory (for example `C:\w\vlite-hp`) to avoid Windows long-path problems.
- One cargo process at a time; keep `target` inside your clone; do not share targets between
  clones.

## 8. Updates to this document

The coordinator may append dated notes below. Run `git pull` on `mahpate-pool-briefs-w4` before each
work session and re-read the notes.

### Coordinator updates

- 2026-11-04: first issue.
- 2026-11-04, 21:45 PT: **HP-1 ruling R-HP1-01 (resolves OPEN-W01).** The op-12 body byte
  grammar is the existing storage format, not the Gap 6 subset implementation, so it is
  authorized ground truth for the write oracle. Extra reading is allowed for format facts only:
  `docs/projects/feature-agent-graph/decisions.md` (the DO-003 body header and entry table, and
  "Gap 3 graph frame rule" for reclaim entries and the prefix `id` and `payload_len` bounds),
  `docs/GRAPH.md`, `docs/projects/roadmap/gap1-c1-consolidation-contract.md` lines 25 and 322 (a
  worked, independently validated size precedent), `src/property_graph/engine/codec.rs` and
  `engine/codec/*.rs` (layouts, constants and size arithmetic only), `engine/properties.rs`
  (property encoding), `engine/mutate.rs` (only to see which entries each `GraphOp` lowers to and
  when reclaim entries are added) and `src/property_graph/frame.rs` (only the op-12 prefix rule).
  Still forbidden: `src/property_graph/subset*`, `src/property_graph/engine/subset/**` and every
  other Gap 6 branch. Safeguards: derive each entry size by hand from the documented table first,
  cite file and line for every constant, then cross-check against `codec.rs`; a mismatch between
  the document and the codec is a new OPEN item for the coordinator, not a choice. Never run or
  transliterate Rust. Expected byte counts stay hand-authored, with the arithmetic in comments.
  To resume: `git fetch origin`, `git switch mahpate-hp1-gap6-write-oracle`, rewrite OPEN-W01 in
  `tests/graph_subset_write_report.md` as RESOLVED with the pinned byte contract, then complete
  section 5.1 in full, including `body-at-cap`, `body-over-cap`, their reclaim-inclusive
  counterparts and the omit-overhead mutant. Keep the branch; the final report commit is last and
  replaces the WIP report.
- 2026-11-04, 22:10 PT: **HP-2 received** (candidate `61f167c9`, under independent review; no
  action needed from that machine unless a fix request is posted here). **HP-5 added** (section
  5.6 and a prompt in section 6): Python only, new files only, any free machine. Status of the
  packages: HP-1 resumed after ruling R-HP1-01; HP-2 delivered; HP-3 and HP-4 are assigned by
  the owner (no branch commits yet); HP-5 is unassigned.
- 2026-11-04, 22:20 PT: **Section 7 amended.** The HP-5 machine correctly stopped at the old
  environment gate, which wrongly demanded Rust for every package. Rust is required only for HP-3,
  HP-4 and verification requests; HP-1, HP-2 and HP-5 need only `git` and Python, and a missing
  `rustc` or `cargo` is not a stop condition for them. HP-5 resumes on its existing branch
  `mahpate-hp5-p3c-ann-oracle` (only a status report sits on top of the base commit).
- 2026-11-04, 22:25 PT: **HP-2 reviewed: ACCEPT-WITH-FIXES.** The fix list and my rulings are in
  `pool-briefs/reviews/hp2-gap1-x1-review.md`. The HP-2 machine appends fix-forward commits on its
  existing branch and ends with a new final report.
- 2026-11-04, 22:40 PT: **Rust is now installed on the worker machines.** HP-3 and HP-4 may proceed
  as written (re-run the section 7 checks first). A new section 9 adds Rust verification requests
  and the kit in `pool-briefs/verify/`; candidate branches `mahpate-cand-gap8-2b` and
  `mahpate-cand-gap6-read-executor` are pushed for them. Never merge those branches.
- 2026-11-04, 23:15 PT: **HP-5 ruling R-HP5-01 (PROVISIONAL) for OPEN-ANN-01.** Adopt your proposal
  with a strict decoder. Persisted scalar minima in the `0x0202` descriptor are canonical: encode,
  and the bind comparison, map a `-0.0` minimum to `+0.0` (bits `0x0000000000000000`); a stored
  `-0.0` minimum (bits `0x8000000000000000`) is MALFORMED; bind compares the stored minima with the
  canonicalized retrained minima. Codes, scales and the public `ScalarU8` do not change (`+0` and
  `-0` both quantize to code 0; constant dimensions keep scale `+0`). Rust's `f64::min` does not
  specify which zero a tie returns, so a Python first-equal minimum would freeze an unstated byte
  rule and is rejected. The contract text lands in section C.3 of `p3c-compact-ann.md` on the
  integration branch; this paragraph is authoritative for you until then. Resume on your existing
  branch: (1) replace the `open` result for ambiguous minima with the canonical rule; (2) goldens:
  a tie dimension holding `+0.0` and `-0.0` in both row orders that produces byte-identical
  descriptors, and an all-zero constant dimension (minimum `+0`, scale `+0`, code 0); (3) malformed
  vectors: `-0.0` minimum at the first, a middle and the last dimension, and a bind negative with a
  stored `-0.0` against a retrained `+0.0`; (4) mutants, all must die with zero survivors under
  plain and `-O`: encode without canonicalizing, a decoder that accepts `-0.0`, a bind that compares
  raw bits, a first-equal minimum, and a canonicalizer that also zeroes a non-zero negative minimum;
  (5) then finish the items in your report (coverage inventory, missing boundary witnesses,
  independent tampered-expected-golden checks). Scope stays C1 (rotation numerics excluded), Python
  only, your six files plus the report; the final report commit is last and replaces the WIP
  report. Push only your branch; no pull request.
- 2026-11-04, 23:40 PT: **HP-2 reviewed again: ACCEPT-WITH-FIXES, last round.** Five of the seven first-round
  fixes are confirmed; two are partial. The remaining fixes are in `pool-briefs/reviews/hp2-gap1-x1-review-2.md`.
  The HP-2 machine appends fix commits on its existing branch and ends with a new report commit. (HP-5 was
  handed to a worker machine at 23:35 PT; do not start it elsewhere.)

## 9. Rust verification requests (machines with Rust)

Read `pool-briefs/verify/README.md` first. A verification request changes no source: the scripts
apply one mutant at a time, run one cargo test command, restore the file and check its hash. Each
request below is complete on its own. Give each machine a different one.

### V-1: Gap 8 step 2b mutation campaign

- Candidate: branch `mahpate-cand-gap8-2b`, SHA `72c42d45ad1f39b859ae7ab12d20b62071cc326f`
  (parent `1acd3ba0`).
- Mutants: `pool-briefs/verify/mut-g8-list.json` with the selection `mut-g8-select.txt` (18 ids),
  run name `g8`.
- Expected: about 14 `KILLED` and 4 `SURVIVED` (G8B2-10, G8B2-14, G8B2-19 and G8B2-20 are predicted
  test gaps). Report every difference from that prediction, and every `COMPILE-ERROR` or `SKIP`.

```text
You are a verification worker on the VLite project (https://github.com/mahpate/Vlite). Rust is installed.
1. Clone the repository (or `git fetch origin`), `git switch mahpate-pool-briefs-w4` and `git pull`.
2. Read pool-briefs/verify/README.md and section 9, request V-1, of pool-briefs/handover-2026-11-04-parallel-pickup.md.
3. Execute ONLY request V-1 exactly as written: candidate branch mahpate-cand-gap8-2b at 72c42d45ad1f39b859ae7ab12d20b62071cc326f, mutants list and selection from pool-briefs/verify.
4. Push only the branch mahpate-hp-verify-g8 with the logs and summary. No other branch, no pull request, no tag, no force-push, no source change.
5. Stop after reporting.
```

### Resume prompt for HP-3 or HP-4 (Rust now installed)

```text
Resume your VLite package (HP-3 cross-version check, or HP-4 default-path audit, whichever you were given). Rust is now installed on this machine.
1. git fetch origin; git switch mahpate-pool-briefs-w4; git pull. Re-read sections 0 to 4 and 7 and the Coordinator updates of pool-briefs/handover-2026-11-04-parallel-pickup.md.
2. Re-run the section 7 tool checks for your package (git, python, rustc, cargo). Then execute your package exactly as written on its own branch (mahpate-hp3-cross-version-check or mahpate-hp4-default-path-audit, from commit 136d72c5a94e1c0d1878f99c461a555a91b7c3f6); keep any branch you already created.
3. Push only that branch, final report commit last. No other branch, no pull request, no tag, no force-push.
4. Stop after reporting.
```
