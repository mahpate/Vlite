# CI and build speed

Status: written with the faster-CI lane. It records where the full CI and the mutation replays spent their time, what
changed, and the working rules that follow. It changes no product behavior and holds no credentials, subscription
identifiers or private infrastructure names.

## Working rules

1. **Run the full suite once per head, on CI.** Dispatch `ci.yml` on the branch (Actions tab, or
   `gh workflow run ci.yml --ref BRANCH`) and read the result. Do not repeat the all-features suite on a laptop or run it a
   second time somewhere else.
2. **Locally, run `cargo fmt --check`, `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` and the
   tests you touched** (`cargo test --test NAME`, or one test by name). That is what CI's `lint` job and your change need;
   `docs/TOOLS.md` lists the complete local set for when you want it.
3. **Dispatch `ci.yml` and a release dry run at the same time**, on the same head: `gh workflow run ci.yml --ref BRANCH` and
   `gh workflow run release.yml --ref BRANCH`. The two workflows have separate concurrency groups, so they overlap instead of
   queueing. Only a `v*` tag publishes, so a branch run publishes nothing; do not start the branch name with `v`, because
   `pypi.yml` reacts to finished release runs whose branch does. A full `ci.yml` run starts about 30 jobs and the account runs
   20 at a time (5 on macOS), so do not start a second full run on top of another branch's: both queue and both slow down.
4. **A head that will ship needs a `ci.yml` run that includes the Docker job.** Pushes skip it unless a container file changed
   (below), and the deployment-image gate wants Docker green for the exact commit: dispatch `ci.yml` on `main` with the
   `docker` input left on.
5. **Replays: shard, then reuse.** Use `mutants.yml` for a whole ledger and `tests/mutant_replay.py run --only-changed` on
   intermediate heads. An independent second-machine replay is needed only at the final head, run without `--only-changed`.
6. **Faster local builds are opt-in.** `. scripts/dev-fast-env.ps1` (Windows) or `. scripts/dev-fast-env.sh` enables sccache,
   lighter debug info and the lld linker where they exist, prints what it did and undoes itself. Nothing is committed to
   `.cargo/config.toml`, so contributors without those tools are unaffected.

## Where the time went

Three runs of `ci.yml` set the baseline: 42183727536 (push to `main`, 42.8 minutes wall) and 42245651707 (44.8 minutes),
both before 0.7.2, and 42298589339 on the 0.7.2 tree (85.9 minutes wall, because it queued behind other runs; its job times
are comparable). Each used more than five runner-hours. One job varies by about 30% between runs, so compare medians.

- The Windows all-features suite was one test step of 29 to 36 minutes (11 to 13 minutes of build, then 4,061 tests two at a
  time in 18 to 23 minutes) inside a 38 to 46 minute job. Linux, macOS and ARM ran the same suite with `cargo test` in about
  18 minutes, and the Linux stable job then ran the default-features suite (about 5 minutes) in the same job.
- The Docker job (31 to 58 minutes, 46 of them in the container suite on the 0.7.2 tree) ran on every push, although only
  container files can change its result.
- Mutation replays ran one mutant at a time per machine from a cold target directory: 0.85 to 2.1 minutes per mutant, so a
  72-mutant campaign took 18 to 48 minutes per machine.

## What changed

| Change | Effect |
|---|---|
| `windows-tests` job: the Windows suite in three Nextest hash buckets per toolchain (`--partition hash:i/3`, `--test-threads 2` each) | Runs 1,369 + 1,391 + 1,301 = 4,061 tests, the baseline count. The small Windows steps stay in `native`, once per toolchain. |
| Docker job only when `Dockerfile`, `compose*.yaml`, `.dockerignore`, `deploy/**`, `scripts/docker*`, the container tests or `ci.yml` change, and on a manual run | `scripts/ci_policy.py` decides in the `changes` job. `release.yml`'s `image-test` is untouched, so every release still runs the container suite. |
| Nextest instead of `cargo test` on Linux, macOS and ARM, plus a separate `cargo test --doc` | Same tests: 4,065 run and 7 ignored. The repository has no doc tests today; the step keeps it that way. |
| `default-features` job: the default-features suite leaves the Linux stable `native` job | Runs in parallel with it (8 to 11 minutes) instead of after it; the same 2,787 tests run, 5 ignored. |
| `.config/nextest.toml`: the five slowest tests start first | They start in the first second instead of up to 8 minutes in. The run did not get shorter (14 minutes on Ubuntu stable either way), because the four vCPUs are the limit; it stops a slow test from starting last. |
| `CARGO_PROFILE_DEV_DEBUG` and `CARGO_PROFILE_TEST_DEBUG` set to `line-tables-only` (manual runs can pick `0` or `2`); Cargo home and TEMP on the local `D:` drive on Windows when there is one | Less debug info to write and link across 158 test binaries. Measured locally: a cold `cargo test --no-run --lib` took 80 s with full debug info, 62 s with `line-tables-only` and 56 s with `0`, and the target directory went from 2.7 GB to 1.3 GB and 1.1 GB. `0` would lose file and line in panic backtraces, which is why `line-tables-only` is the default. |
| `mutants.yml` and `tests/mutant_replay.py` | Shards a ledger over up to 20 runners, replays with the existing tools, merges the verdicts into one table. `--jobs N` runs N isolated worktrees with kept target directories; `--only-changed` skips mutants whose inputs did not change; a mutant whose anchor text no longer matches is reported invalid instead of stopping the shard. |
| `scripts/dev-fast-env.ps1` and `.sh` | Opt-in sccache, `line-tables-only`, lld on Windows and an incremental policy for local builds. |

`ci.yml` has three manual-run options for speed experiments: `docker` (leave the container suite out), `test_proof` (list the
tests and prove that the Windows buckets are disjoint and complete and that Nextest lists what `cargo test` listed) and
`debuginfo`. A run without the Docker job cannot qualify a deployment image.

## Optional: MSRV tests on Ubuntu only

A separate, optional commit (`OPTIONAL gate cut: run the MSRV suite on ubuntu only, check it elsewhere`): revert it to keep
the full MSRV matrix. MSRV 1.89.0 keeps the full all-features suite (Nextest and the doc tests) on `ubuntu-latest`. On Windows,
macOS and ARM it builds every target with all features (`cargo check --locked --workspace --all-targets --all-features`) and
still runs the small per-feature steps; stable stays fully tested everywhere. The Windows buckets run stable only (three jobs
instead of six). What it risks: a test that fails only on the MSRV, and only on Windows, macOS or ARM, is no longer found by
the MSRV run.

## Measured

Minutes per job, from `workflow_dispatch` runs of `ci.yml` on the lane branch. The baseline is the median of 42183727536,
42245651707 and 42298589339. The new column is the median of 42331023206 (cold caches for the new `default-features` job) and
42333040233 (warm), both on the same head with the Docker input off. All 31 jobs of each run passed.

| Job | Baseline | Now |
|---|---|---|
| Windows stable: `native` (now the small steps), then `windows-tests` buckets 1, 2, 3 | 37.5 | 6.6; 11.4, 15.2, 10.0 |
| Windows 1.89.0: `native`, then `windows-tests` buckets 1, 2, 3 | 42.8 | 8.3; 14.6, 10.7, 7.5 |
| Docker | 36.5 (58.0 on the 0.7.2 tree) | not run; 60.6 when run (run 42325667807) |
| macOS stable / 1.89.0 | 20.4 / 29.8 | 22.4 / 20.9 |
| Ubuntu stable / 1.89.0 | 27.4 / 34.3 | 21.6 / 22.4 |
| ARM stable / 1.89.0 | 30.9 / 31.5 | 17.6 / 19.8 |
| `default-features` (a step of the Ubuntu stable job before) | 5.3 | 9.3 |
| Clients on Windows / macOS / Ubuntu | 5.7 / 4.5 / 2.6 | 4.6 / 3.6 / 2.4 |
| **Wall time** | 44.8 | 26.0 |
| **Job minutes** | 315 | 240 |

The longest jobs are now the Linux and macOS cells at 18 to 25 minutes: the 4,065 tests alone take 9 to 14 minutes there. The
next step would be to split those runs into buckets, as the Windows run is.

**Same tests.** Windows runs 1,369 + 1,391 + 1,301 = 4,061 tests across the buckets, the 4,061 that one unsplit Nextest run
started on the 0.7.2 tree, and the proof step reports the buckets disjoint and complete. Linux, macOS and ARM run 4,065 (7
skipped) in 158 binaries, as the baseline's `cargo test` did (4,065 passed, 7 ignored, 158 results), and `cargo test --list`
and `nextest list` name the same 4,072 tests in every binary. The `default-features` job runs the 2,787 tests (5 ignored) of
the step it replaces.

**Mutation replay.** `mutants.yml` on `tests/k71_mutants.json` (206 mutants, 20 shards, cold caches, run 42286695573) took
9.1 minutes wall and 110 runner-minutes: 203 killed, 0 survived and 3 invalid. The last serial campaign needed 0.85 to 2.1
minutes per mutant on each machine. The three invalid entries (`lines-fix/lf-mcp-cap-64-mib`, `lines-fix/lf-mcp-cap-minus-one`
and `import/cap-8-mib`) edit a line cap that a compile-time assertion guards, so the mutated build stops before any test runs.
`tests/k71_mutants.py` calls that INVALID, and the assertions are older than the ledger: those three need a different edit,
not a different run.

## Known gaps

- Linux, macOS and ARM now run the all-features suite with one process per test, as Windows already did. The threaded
  `cargo test` form still runs in the `default-features` job (default features, Linux stable) and in `release.yml`.
- Nextest runs tests of different binaries at the same time, which `cargo test` does not. No new flake showed in the green
  runs above, but a test that shares a fixed port or file name across binaries would now be exposed.
- The Docker gate compares `before..sha` of each push. A push whose run the next push cancelled can hide a container-file change
  from the next push's diff; the image gate and the release's `image-test` still cover it (rule 4).
- The first run after a change of the cache key is slow once, because `rust-cache` starts cold.
- `.config/nextest.toml` needs cargo-nextest 0.9.91 or newer; an older one stops with a message to update it. The container
  suite does not read it: the Dockerfile copies no `.config`.
- The `sccache`, lld and zsh paths of `scripts/dev-fast-env.*` were not measured, and the build times above come from one
  machine.
- The three invalid `tests/k71_mutants.json` entries still need a different edit.

## Optional: the release skips its native test re-run

A separate, optional commit (`OPTIONAL gate cut: release skips its native test re-run when CI is green for the commit`):
revert it to keep the re-run. `release.yml` gets a read-only `ci-green` job that asks the Actions API for a successful
`ci.yml` run (push or dispatch) of the same commit in which every stable test job succeeded: the four native cells and the
three Windows buckets. When there is one, the `native` and `linux-arm64` jobs skip their `cargo test` step; with none, or if the
API cannot be read, they run it as before. Formatting, clippy, builds, wheels, the container suite and publishing are
unchanged, and the publish jobs still run on a `v*` tag only. Tag a commit after its CI run is green to get the saving: a tag
pushed while CI is still running just runs the tests, as it always did.

What it drops: the release's threaded `cargo test --all-features` run on Windows, macOS, Linux and ARM. With the Nextest
change, `ci.yml` runs that suite with one process per test everywhere, so a failure that shows only when tests share a
process (global state, a fixed port) would be found only by the release's `image-test` (its image build runs the threaded
suite on Linux x86_64), the `default-features` job (default features) and local runs.

What it saves: runner minutes, not wall time. `image-test` is the longest release job (30 to 48 minutes), so a dry run went
from 175 job minutes and 43.0 minutes wall with the tests repeated (run 42335783932) to 84 and 39.8 (run 42346683352).
One earlier dry run of the same commit (42337799041, cancelled) spent over 100 minutes in `image-test`: its second image build
did not reuse the first build's layers, so the container suite ran twice. If that job runs past an hour, cancel it and
dispatch again.
