# HP-3 report: cross-version compatibility and behavior check

- Branch `mahpate-hp3-cross-version-check`. Base `136d72c5a94e1c0d1878f99c461a555a91b7c3f6`.
  Candidate `7bbb3dc83b8ebe5a75c27f06f3701243995faed6`.
- Added: `scripts/cross_version_probe.rs` (1,190 lines), `scripts/cross_version_check.py` (573),
  `tests/fixtures/cross_version/` (24 hex files plus `MANIFEST.sha256`; 51 lines). Total 1,814.

**Result: 0 findings.** Matrix A has 16 differences, all justified (directory store). Matrix B is
81 of 81 identical. Matrix C: `v0.6.1` refuses every file carrying a flag it does not know with
`UnsupportedFeature`, and refuses every directory; both refuse flag 128.

**Modeled.** Everything 5.3 lists for Matrix A:
- all three metrics with ties, filters, hybrid search, browse and pagination, batches;
- TTL via the `_at` variants and `set_commit_clock`;
- compaction, backup and restore, the change feed;
- an encrypted file and a directory store.

Matrix B covers nine file kinds (`b_v3`, `b_v4`, `b_v5`, `b_ts`, `b_mem`, `b_compact`, `b_restore`,
`b_torn`, `enc_b`). Matrix C uses 26 byte-built inputs.

**Not modeled.** `v0.6.0` (optional); Linux and macOS; candidate-only APIs (by design).

**Needs ratification.**
1. The brief lists flag 4 as new, but GRAPH is known since v0.5.1, so `v0.6.1` accepts `f_4`. It is
   labeled "accepted by both", not a finding.
2. The candidate refuses synthetic HISTORY and COMPACT files with `Corrupt` (structural check
   first), not `UnsupportedFeature`. This is informational, since the candidate knows these flags.
3. `v0.6.1` refuses a directory with an OS error (`Io(PermissionDenied)` on Windows). The text is
   platform-dependent, so the harness requires only `err:`.

**Mutation campaign.** 16 killed, 0 survived, 0 equivalent, under plain Python and under `python -O`.
Each mutant rewrites one probe output stream through a throwaway wrapper around `probe()`, which is
not committed. The evidence list names each one.

**Deviations.**
1. Build: the precedent compiles a separate probe crate without `--locked`. Here the probe is
   copied into each `git archive` extraction as `src/bin/cross_version_probe.rs` and built with
   `cargo build --locked --offline --bin cross_version_probe --target-dir <work>/target-<rev>`,
   so each revision's own lockfile applies. Default features are used and the working tree is
   never touched.
2. The directory store and the flagged files are byte-built, because the `v0.6.1` API has no
   directory constructor.
3. Encrypted Matrix C variants are derived at run time from a `v0.6.1`-encrypted file (random
   nonces): the flags are re-set and the header CRC is recomputed. Only plain fixtures are
   committed.

**Proposed CI lines (not applied):** add to `ci.yml` `native`, stable, ubuntu-latest, with
`fetch-depth: 0` for the tag:

```yaml
- run: python scripts/cross_version_check.py fixtures --check
- run: python scripts/cross_version_check.py run --json cross-version.json
```

**Not done.** `v0.6.0`; non-Windows runs; committing the mutation wrapper (line budget).

## Evidence

Toolchain: rustc 1.99.1 (58cf8f9c3 2026-10-13), cargo 1.99.1 (5f94df478 2026-08-27), Python
3.13.17, git 2.56.0, win32, 16 CPUs, `CARGO_INCREMENTAL=0`, at most 8 jobs. Reference `v0.6.1` is
`fb46702f6c14a6a2d9de750858cf1c14503fc563`.

| Command | Exit | Counts |
|---|---|---|
| `python scripts/cross_version_check.py fixtures` / `fixtures --check` | 0 / 0 | 24 fixtures, 25 manifest-checked files |
| `python -O scripts/cross_version_check.py --help` | 0 | 3 subcommands |
| clean detached checkout at `7bbb3dc`: `python -O scripts/cross_version_check.py run --json clean.json` | 0 | 61 s cold, 179 commands all exit 0, 0 findings, tree still clean |
| warm rerun `run --work <dir> --json result2.json` | 0 | Matrices A, B and C identical to the clean run |
| `compare` of real transcripts / of a tampered one | 0 / 1 | 16 justified / 17 differences, 1 finding |
| `cargo build --locked --offline` (each revision) | 0 | 0 warnings |
| `git diff --check`; `git diff --name-status 136d72c` | 0 | only `A` lines |
| mutation wrapper, plain / `-O` | 0 / 0 | 16 of 16 killed each |

**Matrix A.** 273 operations (`v0.6.1`) vs 288 (candidate). Each workload ran twice per revision,
with identical transcripts. The 16 differences are the `dir.*` rows: `v0.6.1` `dir.open` returns
`err:Io(... "Access is denied.")`. The candidate opens the byte-built store (`ok v=5 len=15`) and
supports put, delete, reopen and get. A second writer gets `err:Locked` and a read-only put gets
`err:ReadOnly`. Every other operation is identical.

**Matrix B.** 81 checks, all identical:
- produce: 9 of 9 on both;
- produced bytes, CRC and lineage masked: 8 (`enc_b` is excluded because of its nonces);
- reads: 54 (each produced or appended file dumped by both revisions);
- append transcripts: 2 × 90 operations;
- appended-copy bytes: 16.

Both revisions refuse `b_torn` the same way: `IncompleteTail { offset: 2756, bytes: 11 }`.

**Matrix C** (`open_read_only`; `open`, `open_from_bytes` or `inspect`, and `verify` agree):

| Input | `v0.6.1` | Candidate |
|---|---|---|
| `f_0`, `f_4`, `encf_5` | ok | ok |
| `f_8`, `f_12` | `UnsupportedFeature(8)` | `Corrupt{32}` history image without advance frame |
| `f_16` | `UnsupportedFeature(16)` | `Corrupt{0}` compact flag without history image |
| `f_24`, `f_28` | `UnsupportedFeature(24)` | `Corrupt{32}` history image |
| `f_32` | `UnsupportedFeature(32)` | `UnsupportedFeature(32)` |
| `f_60` | `UnsupportedFeature(56)` | `UnsupportedFeature(32)` |
| `f_64`, `f_68` | `UnsupportedFeature(64)` | `UnsupportedFeature(64)` |
| `f_128`, `f_132` | `UnsupportedFeature(128)` | `UnsupportedFeature(128)` |
| `encf_13`, `encf_29` | `UnsupportedFeature(8)`, `(24)` | `Corrupt{96}` history image |
| `encf_61`, `encf_69`, `encf_133` | `UnsupportedFeature(56)`, `(64)`, `(128)` | `UnsupportedFeature(32)`, `(64)`, `(128)` |
| `d_4`, `denc_5` | `Io(PermissionDenied)` | ok |
| `d_36`, `d_68` | `Io(PermissionDenied)` | `UnsupportedFeature(32)`, `(64)` |
| `d_132`, `denc_133` | `Io(PermissionDenied)` | `Corrupt{0}` log segment header carries image flags |
| `d_image_12` | `Io(PermissionDenied)` | `Corrupt{32}` history image |

**Mutants (all killed):**
- A1: candidate search result altered.
- A2: candidate drops a `get`.
- A3: reference `dir.open` accepted, which makes the directory rows unjustified.
- A4: candidate workload crash.
- B1: `v0.6.1` misreads a candidate `b_v5` record.
- B2: candidate misreads a `v0.6.1` `b_ts` record.
- B3: candidate refuses `b_v4`.
- B4: candidate append upsert errs.
- B5: candidate partial produce.
- C1: `v0.6.1` accepts `f_16`.
- C2: `v0.6.1` refuses `f_8` with `Corrupt`.
- C3: candidate accepts `f_128`.
- C4: candidate crashes on `f_0`.
- C5: `v0.6.1` fails `encf_5` verify.
- C6: `v0.6.1` accepts `d_4`.
- C7: `v0.6.1` crashes on `encf_13`.
