# T20 - Native Windows live Azurite verification

**TEST-006 live: PASS (3/3)**

**Overall result: FAIL, not an all-green HA run.** All requested build and test commands were executed. Reader archive-apply passed three live runs, cloud backup/restore passed, and all 11 offline regression tests passed. The gateway control failed reproducibly on the dedicated tenant's collection creation: HTTP 502 instead of 201. No product code or tests were changed.

## Environment and scope

| Item | Observed value |
|---|---|
| Execution date | 2026-11-18 local, UTC-08:00; 2026-11-19 UTC |
| Machine | `CPC-mahpa-Z9S8K` |
| OS | Microsoft Windows 11 Enterprise, 64-bit, `10.0.26200` |
| Checkout SHA | `8ad6dd6e54570cb9f56bc604f991a0180b4365c3` |
| Rust | `rustc 1.99.1 (58cf8f9c3 2026-10-13)` |
| Cargo | `cargo 1.99.1 (5f94df478 2026-08-27)` |
| Rust host | `x86_64-pc-windows-msvc` |
| Node | `v26.13.0` |
| npm | `11.20.0` |
| Azurite | `3.37.0` |
| Blob endpoint | `http://127.0.0.1:10010/devstoreaccount1` |
| Platform restrictions | Native Windows/Node only; no Docker, WSL, Azure deployment, or shared-machine configuration changes |

This report follows `pool-briefs\T20.md`. Sources read before execution included `docs\projects\feature-aca-ha\gateway-test-plan.md` (especially "Tier (b) execution"), all three Azurite test files, and `docs\projects\feature-aca-ha\evidence\epic-004-green.txt`. That historical green log describes another revision and is not validation evidence for this SHA.

## Execution method and necessary deviations

All generated artifacts were isolated beneath:

```text
C:\Users\mahpate\AppData\Local\Temp\vlite-t20-faceafbdbf4f4add87854bf58676ec7f
```

`CARGO_HOME`, `CARGO_TARGET_DIR`, npm's cache, `TEMP`, `TMP`, runtime fixture working directories, captured logs, and Azurite data were directed there. The complete directory was deleted after execution and evidence collection.

The brief's literal `npx --yes azurite-blob --version` failed with npm E404 after 81.702 seconds: `azurite-blob` was not an available package in the configured registry. The executable is supplied by the **azurite** package. The successful fallback was:

```powershell
npx --yes --package azurite azurite-blob --version
# 3.37.0
```

The task-installed Blob entry point was then started directly with Node, avoiding another npm lookup:

```text
node.exe <task-installed-azurite>\dist\src\blob\main.js
  --blobHost 127.0.0.1 --blobPort 10010
  --location <TASK_TEMP>\azurite
  --skipApiVersionCheck --disableTelemetry --loose
```

Azurite PID was `4604`. A bounded native TCP probe confirmed readiness, followed by an HTTP request that returned 403 as expected without authentication. Startup/readiness took 4.537 seconds; stdout confirmed it listened on `127.0.0.1:10010`. The emulator was demonstrably responsive, not merely a launched process.

All Cargo commands below used the requested feature/test selectors, with the additional `--locked` flag to prohibit lockfile changes. A temporary Cargo runner was configured through `CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER`. Its only behavior was to change to `<TASK_TEMP>\runtime`, execute the original compiled test binary with its original arguments, and propagate its exit code. This matters because `tests\gateway_azurite.rs:564` and `tests\cloud_backup_azurite.rs:249` explicitly use `tempfile::tempdir_in(".")`; changing only `TEMP` would still create fixtures inside the checkout. The runner did not modify tests, assertions, timeouts, or binaries.

The isolated build and npm download overlapped. Offline controls ran after the build while npm setup finished; live tests then ran sequentially against the verified emulator. No live suites overlapped.

## Checks actually executed

Wall times below are measured around each complete Cargo invocation, including runner startup. Harness times are the test binary's own reported durations.

| Command/test | Run | Result | Exit | Wall seconds | Harness seconds |
|---|---:|---|---:|---:|---:|
| `cargo build --locked --features server --tests --bins` | 1 | PASS | 0 | 455.645 | n/a |
| `reader_apply_azurite` live | 1 | PASS, 1 test | 0 | 71.866 | 70.44 |
| `gateway_azurite` live | 1 | FAIL, 1 test | 101 | 2.549 | 1.00 |
| `cloud_backup_azurite` live | 1 | PASS, 1 test | 0 | 27.134 | 25.61 |
| `reader_apply_azurite` live | 2 | PASS, 1 test | 0 | 71.733 | 70.56 |
| `reader_apply_azurite` live | 3 | PASS, 1 test | 0 | 71.778 | 70.56 |
| `gateway_azurite` diagnostic live repeat | 2 | FAIL, same assertion | 101 | 2.213 | 1.07 |
| `server_reader_apply_cli` offline | 1 | PASS, 4 tests | 0 | 21.245 | 12.25 |
| `server_failover_cli` offline | 1 | PASS, 1 test | 0 | 1.344 | 0.15 |
| `server_serve_mode` offline | 1 | PASS, 6 tests | 0 | 8.357 | 0.04 |
| Gateway epoch unit-test filter, local fixtures only | 1 | PASS, 2 tests | 0 | 8.398 | 0.02 |

The live command shape, executed separately for each test/run, was:

```powershell
$env:VLITE_AZURITE_BLOB_ENDPOINT = 'http://127.0.0.1:10010/devstoreaccount1'
cargo test --locked --features server --test reader_apply_azurite -- --ignored --nocapture
cargo test --locked --features server --test gateway_azurite -- --ignored --nocapture
cargo test --locked --features server --test cloud_backup_azurite -- --ignored --nocapture
```

Reader apply was repeated twice more with the same command. Gateway was repeated once more for diagnosis. Offline commands were:

```powershell
cargo test --locked -q --features server --test server_reader_apply_cli
cargo test --locked -q --features server --test server_failover_cli
cargo test --locked -q --features server --test server_serve_mode
```

### TEST-006 observations

Each run executed the ignored test rather than skipping it: output said `1 passed; 0 failed; 0 ignored`. The test's measured insert-batch-to-reader-visibility output was:

| Reader run | Visibility milliseconds | Margin below 100,000 ms apply-wait budget |
|---|---:|---:|
| 1 | 59,977 | 40,023 ms |
| 2 | 60,212 | 39,788 ms |
| 3 | 60,175 | 39,825 ms |

The budget derives from `tests\reader_apply_azurite.rs:502-504`: 60-second archive interval + 30 seconds + 10-second staleness bound. Visibility is measured from before the two writer inserts (`:494-523`), so the margins above are approximate, conservative comparisons rather than instrumentation of the exact `eventually` entry timestamp.

No wait expired. The "running for over 60 seconds" notices were informational, not timeouts. Approximately 60-second visibility is consistent with the deliberately configured archive interval (`:25`, `:422-430`), not evidence of a 10-second commit-to-reader guarantee. The staleness age is time since confirmation of the newest verified archive checkpoint (`src\server\reader_apply.rs:26-30`, `:55-60`), not time since every writer commit.

Passing assertions covered published-set enrollment, initial reads and position/age headers, new records appearing with advanced sequence, mutation refusal with `reader_mode`, and fail-closed readiness/data responses after stopping the writer and deleting its data container. The final assertion permits either `reader_stale` or `reader_apply_failed` (`tests\reader_apply_azurite.rs:529-545`); successful output does not identify which branch occurred. Do not interpret these runs as proof of the specific `reader_stale` branch alone.

## Gateway failure evidence

Both runs failed at `tests\gateway_azurite.rs:806:5`, in the assertion spanning `:806-816`. The dedicated tenant's `POST /v1/collections` returned **502**, while the test expected **201**. This was an immediate status mismatch, not a timed-out readiness or propagation wait.

The following are the complete captured final outputs (each shorter than 200 lines). Only the long external build-directory prefix is normalized to `<TASK_TEMP>`; assertions, exit diagnostics, and reported durations are retained.

### Run 1 - final output

```text
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.47s
     Running tests\gateway_azurite.rs (<TASK_TEMP>\target\debug\deps\gateway_azurite-c54956bf48e398a6.exe)

running 1 test

thread 'gateway_and_trusted_backends_serve_isolate_and_fail_closed' (30344) panicked at tests\gateway_azurite.rs:806:5:
assertion `left == right` failed
  left: 502
 right: 201
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test gateway_and_trusted_backends_serve_isolate_and_fail_closed ... FAILED

failures:

failures:
    gateway_and_trusted_backends_serve_isolate_and_fail_closed

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s

error: test failed, to rerun pass `--test gateway_azurite`
```

### Run 2 - final output

```text
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.48s
     Running tests\gateway_azurite.rs (<TASK_TEMP>\target\debug\deps\gateway_azurite-c54956bf48e398a6.exe)

running 1 test

thread 'gateway_and_trusted_backends_serve_isolate_and_fail_closed' (8064) panicked at tests\gateway_azurite.rs:806:5:
assertion `left == right` failed
  left: 502
 right: 201
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
test gateway_and_trusted_backends_serve_isolate_and_fail_closed ... FAILED

failures:

failures:
    gateway_and_trusted_backends_serve_isolate_and_fail_closed

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.07s

error: test failed, to rerun pass `--test gateway_azurite`
```

### Diagnosis - static inspection, not a captured live response body

**Most likely classification: product integration defect / missing dedicated-backend epoch producer, exposed by the existing end-to-end control.** The consumer correctly fails closed, but the currently wired producer does not provide the response contract it requires. Evidence:

1. The fixture starts the dedicated backend using `backend()` with only bind/trust arguments (`tests\gateway_azurite.rs:370-379`, `:701`). Its dedicated directory record explicitly advertises `"epoch":1` (`:709-710`).
2. The real gateway checks every epoch-bearing target's upstream response before streaming it (`src\server\gateway_mode.rs:511-535`). It parses `x-vlite-epoch`, obtains the freshest same-lineage directory record, and refuses a missing or insufficient stamp.
3. Missing stamps are refused by `check_epoch` (`src\server\gateway.rs:553-562`); a refused write becomes `502 write_outcome_unknown` (`:515-520`, `:535-547`), matching the observed status.
4. A source-wide `*.rs` search under `src` for `x-vlite-epoch`, `X-VLite-Epoch`, and writer-epoch/stamping identifiers found the gateway parser/consumer and test-fixture headers, but no production response-header producer. The backend startup path wires serve mode and gateway trust, with no epoch attachment (`src\bin\vlite-server.rs:921-933`).
5. Shared-pool equivalence/write/isolation assertions preceding the dedicated request passed far enough to reach line 806. Cloud backup and all reader runs also passed against the same emulator. This is inconsistent with a general Azurite availability or signing failure.

**Uncertainty:** the failing assertion retains only `.status`; it does not print the response body/headers or drain the gateway's captured stderr. Thus `write_outcome_unknown` and the absent stamp on this exact live response are strongly supported by static code, but were not directly recorded from that response. Another 502 path cannot be excluded solely from the failure log. No timing-only explanation was observed.

### Additional executed diagnostic checks - fixture-only

```powershell
cargo test --locked --features server --lib server::gateway_mode::tests::gateway_epoch -- --nocapture
```

This executed two unit tests, both passing:

```text
{"code":"backend_epoch_stale","event":"gateway_upstream_failed","operation":"write"}
test server::gateway_mode::tests::gateway_epoch_refusal_never_retries_write ... ok
{"code":"backend_epoch_stale","event":"gateway_upstream_failed","operation":"read"}
test server::gateway_mode::tests::gateway_epoch_refusal_does_not_fall_back_to_writer ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 2033 filtered out; finished in 0.02s
```

These are local fixture checks, not a passing live gateway test and not proof of the precise live error body.

## Recommendations and limits

Restore the dedicated-backend response contract with an authoritative epoch producer consistent with the fencing design, then rerun the unchanged live gateway test. Do not remove the directory epoch or relax the gateway's fail-closed check merely to obtain a green control. If standalone-backed fixtures intentionally cannot provide that contract, explicitly revise their topology and retain genuine dedicated/fenced integration coverage. Improve the failing assertion to print a sanitized response code/body so future failures distinguish epoch refusal from transport errors; no such edit was made here.

The historical gateway test plan's "No fencing" and "Epoch response header ... waits for OD-002" limitations now differ from the current gateway consumer implementation (`src\server\gateway_mode.rs:29-31`, `:511-535`). Reconcile that documentation with the intended producer/consumer rollout rather than treating historical green evidence as current.

No required build/test suite was omitted or blocked after the package-name workaround. However, the failing gateway test stops before its later restart, rate-limit, revocation/suspension, relay, drain, outage, and generation-regression assertions. **Those later live behaviors were not validated in this run.** The offline regression and epoch-fixture passes do not replace them.

Three reader successes establish repeatability only for this native Windows emulator setup. They do not qualify ACA ingress, managed identity, RBAC/immutability, replica scaling, fencing, production load, a measured production staleness bound, GATE-003, or MEAS-004/005/007. No full test suite, clippy, or formatting run was requested or executed.

## Checkout integrity and cleanup

Before creating this report, `git status --short --untracked-files=all --ignored`, `git diff --stat`, and `git diff --cached --stat` all produced no output. Thus source, tests, lockfile, and other reports were unchanged; there were no generated ignored artifacts in the checkout.

Azurite process ownership was verified from its Node command line, then PID **4604** was stopped by ID. A subsequent process scan found **zero** task-owned Node/server/Cargo/rustc processes. A TCP connection to port **10010** then failed, confirming the listener was gone. Azurite's data directory was deleted and its absence verified. The entire task-specific temporary directory, including build output, package caches, fixtures, runner, and raw logs, was deleted and its absence verified.

The sole intended checkout change is this report, `reports\330edc62cfff49c3868359c0c785ec8a\faceafbdbf4f4add87854bf58676ec7f.md`. A literally empty final status would contradict delivering that new report; the clean-checkout confirmation above applies immediately before its creation. No branch switch, commit, or push was performed.
