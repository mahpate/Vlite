# T31 (attempt 2): disposable ACA MEAS-004/005/006 measurement harness

You are implementing and offline-testing a disposable Azure Container Apps (ACA)
measurement harness for the opt-in VLite HA feature. The repository owner wants
HA finished on this branch; temporary, tagged measurement apps with guaranteed
cleanup are authorized, and NO new external dependencies may be added (no Key
Vault, no new crates, no new Python/PowerShell modules). You implement and
run mocked tests only: NEVER call real Azure, never provision anything, never
open pull requests. The parent session runs Azure later from the pushed commit.

## Why this is attempt 2 (read carefully)

Attempt 1 failed at merge with "Out-of-scope changes (17 paths)" because it
merged a prerequisite task's result. There is NO prerequisite now: the
runtime-driver work (commit 0644fa0a) is ALREADY in your base. It contains:

- `src/server/gateway_measure.rs`, `src/server/gateway_mode.rs` and the
  `vlite-server gateway-measure` subcommand (`bootstrap` builds an offline
  private fixture with authority off and `writer_only`; `mint` produces a
  signed envelope);
- `benchmarks/ha_meas004.rs` (ABBA load driver), `benchmarks/ha_meas005.rs`
  (observers + `analyze`; percentiles stay null without clock calibration),
  `benchmarks/ha_gateway_support.rs`;
- `deploy/azure-container-apps/ha/measure-gateway-contract.md` (the CLI
  contract you must follow, not invented placeholders);
- root `Dockerfile` stages `gateway-measure-build` / `gateway-measure`.

Do NOT modify, recreate, regenerate or copy any of those. Your diff must touch
ONLY the exclusive write scopes below. Any other changed or untracked path left
in the checkout (including Cargo.lock churn, generated files, scratch copies,
`target/`, logs, `pool-briefs/`) fails the whole task and discards your result.
Put scratch output outside the checkout. If the contract genuinely prevents
execution without an out-of-scope change, do not make it: implement everything
else and report the precise blocker (file, line, needed change) in your final
commit message and result.

## Exclusive write scopes (only these paths)

- `deploy/azure-container-apps/ha/measure-gateway.ps1` (new)
- `deploy/azure-container-apps/ha/measure-gateway.test.ps1` (new)
- `deploy/azure-container-apps/ha/measure.ps1`
- `deploy/azure-container-apps/ha/measure.test.ps1`
- `deploy/azure-container-apps/ha/measure.dockerfile`
- `deploy/azure-container-apps/ha/measure.md`
- optional `deploy/azure-container-apps/ha/measure-gateway-entrypoint.py` and
  `deploy/azure-container-apps/ha/measure-gateway-entrypoint.test.py`

Forbidden: `deploy.ps1`, `templates.ps1`, `README.md`/`test.ps1` under
`deploy/azure-container-apps`, any PRD/docs outside the list, Rust sources,
benchmarks, root `Dockerfile`, site/dashboard. New image build steps belong in
`measure.dockerfile`, not the root `Dockerfile`.

## Read first

- `pool-briefs/T30c-design.md` (design proposals, NOT evidence; adapt to the
  real contract where they disagree).
- `deploy/azure-container-apps/ha/measure-gateway-contract.md`.
- Existing `measure.ps1`, `measure.test.ps1`, `measure.dockerfile`,
  `measure.md`, `meas006-collect.sh`, and the pure `templates.ps1`.
- `src/server/writer_fence_bench.rs` (MEAS-006 environment contract).

## Deliverable A: MEAS-004 / MEAS-005 gateway harness

`measure-gateway.ps1` (+ dispatch/image support in the existing measure files)
must make end-to-end execution possible from clean pinned source with ONE
command per run: MEAS-004 overhead, MEAS-004 scale, or one MEAS-005 poll
interval. Expose bounded pilot duration/counts and record the effective values
in the evidence. Preserve existing MEAS-007/MEAS-002 behavior exactly. Do NOT
bypass the HA deployment qualification verifier: this is an independent
disposable measurement path only.

Resource rules:
- New resource group only, exact owner/run/expiry tags; refuse protected names
  `vlite-app`, `vlite-env`, `vlite-data`, `asendme-rg` (and `asendme*`) and any
  preexisting or foreign resource or group.
- ACR Basic with admin disabled; user-assigned managed identity with
  role-specific, container-scoped grants; HTTPS-only `Standard_ZRS` storage
  with shared-key access and public access disabled; isolated ACA environment.
- Gateway apps fixed at 2 replicas for overhead runs and MEAS-005; the scale
  run uses min 2 / max 4 and the runtime max must match.
- Trusted `writer1` internal-only with a private ephemeral fixture copied from
  the image (a restart invalidates the trial); `reader1` for MEAS-005 only if
  the contract requires it.
- Existing ACA secrets only; private generation-suffixed manifests. Bootstrap
  fixture/authority/control before serving, using the contract; no repeated
  uncertain init.
- Same-environment manual jobs, timeout <= 3600 s, retry 0. Bound every retry
  and wait.

Evidence rules:
- Collect a concurrent 5 s revision-scoped replica inventory WHILE the job
  runs, not after.
- Preserve raw, verified gzip/base64/sha256 driver output (reuse the existing
  `VLITE-MEAS BEGIN/<n>/END` transport and parser); write derived evidence
  (scale reaction intervals, MEAS-005 replica coverage, clock uncertainty) to
  separate files.
- Unknown, missing, stale or partial outcomes are explicit values, never zeros.
  Full per-replica convergence requires snapshot + floor and stable identities;
  an unbounded cross-host clock means timing is reported incomplete.
- Bounded logs/inventory; no credentials, native diagnostics, subscription,
  tenant, principal or client IDs in evidence.
- Always request group deletion in `finally` and wait for `group exists` to
  return exit 0 with boolean false; keep measurement failure and cleanup failure
  as separate statuses. No `KeepOnFailure` for the new modes.

## Deliverable B: MEAS-006 on ACA (small; same files, so it is yours too)

`measure.ps1` currently rejects MEAS-006. Add a MEAS-006 mode that runs the
existing writer-fence probe against real Azure Blob storage:

- Image: in `measure.dockerfile` also build the lib unit-test executable with
  `cargo test --locked --release --features server --lib --no-run` (locate the
  produced `target/release/deps/vlite-*` test binary deterministically, e.g.
  via `--message-format=json`, and copy it to a fixed path), copy
  `deploy/azure-container-apps/ha/meas006-collect.sh`, and add a `MEAS-006`
  case in the `vlite-meas` wrapper that runs
  `meas006-collect.sh <fixed test binary path>` so its single JSON document
  goes through the existing gzip/sha256/base64 transport.
- Contract (`src/server/writer_fence_bench.rs`, do not edit it):
  `VLITE_MEAS006=1`; `VLITE_MEAS006_ACA_RUN_ID=<Id>` (3-12 lowercase
  alphanumerics); `CONTAINER_APP_JOB_NAME` must equal `vlite-meas-<Id>-c1`, so
  MEAS-006 uses exactly the 1 vCPU tier; storage account `vlitemeas<Id>st`
  with container `coordination` (prefix `meas006/` is chosen by the probe);
  managed identity via `VLITE_MEAS006_CLIENT_ID` (the identity's client ID,
  injected at job creation, never written to evidence or console);
  optional bounded `VLITE_MEAS006_SAMPLES` (1-2000, default 100),
  `VLITE_MEAS006_REPETITIONS` (1-100, default 20),
  `VLITE_MEAS006_INTERVAL_MS` (5-5000, default 50) exposed as validated
  parameters; `VLITE_RESOURCE_SOURCE_REVISION` and
  `VLITE_RESOURCE_SOURCE_DIRTY_PATHS` (the probe refuses a dirty source in an
  ACA job, so refuse `-AllowDirty` for MEAS-006). Storage for MEAS-006 is
  `Standard_ZRS`, shared key and public access off, HTTPS-only, TLS1_2, with a
  `Storage Blob Data Contributor` grant scoped to the `coordination` container.
  Do not change MEAS-002's existing storage behavior.
- Add mocked tests for the new mode (plan shape, env vars, parameter bounds,
  dirty refusal, job name, no client ID in evidence/summary).

## Tests and validation (all must pass before you commit)

- Mocked tests for: preexisting/foreign resources, protected names, redaction,
  malformed/unknown observations, checksum/schema/provenance failures, failure
  cleanup, replica changes, missing coverage, MEAS-006 mode, and unchanged
  MEAS-007/MEAS-002 behavior. Verify argument syntax and JSON shapes in mocks.
  Do not weaken observation gates to force success.
- Run: `pwsh -NoProfile -File deploy/azure-container-apps/ha/measure.test.ps1`
  and `pwsh -NoProfile -File deploy/azure-container-apps/ha/measure-gateway.test.ps1`
  (and the Python entrypoint tests if you add them).
- Docker build-input check: every `COPY` source in `measure.dockerfile` exists
  in the repo and is not excluded by `.dockerignore`.
- `git diff --check`; `git status --porcelain` must list only write-scope paths.

## Commit

One commit (or a few) touching only the write scopes, message starting
`T31: `, ending with the trailer
`Co-authored-by: Copilot App <223556219+Copilot@users.noreply.github.com>`.
In the commit body and in `measure.md`, give the exact one-command usage for:
MEAS-004 overhead, MEAS-004 scale, MEAS-005 at one poll interval, MEAS-006,
and a `-Plan` smoke command for each.
