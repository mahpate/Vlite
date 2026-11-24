# Temporary ACA measurement jobs

MEAS-007 packages the existing reader-apply resource probe for the
owner-approved **separate, temporary measurement apps**. It creates a new,
tagged resource group containing an ACR, user-assigned identity, Consumption
environment (with Log Analytics), and one Manual job per requested CPU tier.
It never reuses a group or touches the live app, environment, or share.
MEAS-002 also measures reader staleness under write load against real Blob
Storage, and MEAS-006 measures writer-fence costs against a temporary Blob
coordination container. **MEAS-001 and MEAS-003 are still refused**, as are combinations of measurement IDs. MEAS-004/005 have a gateway
container runtime (`measure-gateway-entrypoint.py`, packaged with `python3` in
the `gateway-measure` image) and a contract (`measure-gateway-contract.md`). The
offline Linux/WSL exact-fixture preparation (`measure-gateway-fixture.py`) is
also in place. It bootstraps once at `/measurement/bootstrap`. It then exports
and verifies the four role bundles and writes a private image context for that
bootstrap's own `database.vlite`. It plans the fixture image `az acr build`.
`measure.ps1` now also contains MEAS-004/005 orchestration
(`measure-gateway.ps1`; see *MEAS-004/005 gateway orchestration* below). It is
tested offline. First live attempts on 2026-11-20/21 produced no evidence:
MEAS-004 failed at `BuildMeasureImage` with `Unclassified`, and MEAS-005 was not
started. The MEAS-006 run from clean revision `467f54d` failed at `ReadLogs`
with `JobFailed` / `FailedMarker` (exit 2); Log Analytics was not queried. Its
cleanup wait timed out, but a later read-only check confirmed the group absent.
MEAS-004 cleanup and group absence were also confirmed. Hash-bound cleanup
follow-ups were preserved beside the unchanged summaries. These failures do
not qualify HA or SLOs. The gateway image build now uses an ACR task with
`DOCKER_BUILDKIT=1`. A build-only run verified the new path in ACR; it did
not run a measurement. The MEAS-006 collector is pinned to LF by
`.gitattributes`; that fix has only been verified offline. The context
explicitly excludes `pool-briefs/`. Offline tests:
`python3 -B measure-gateway-fixture.test.py`
(Linux/WSL) and `pwsh -NoProfile -File measure-gateway.test.ps1`. The second
suite mocks every Azure CLI call, local process, clock and sleep.

## Prerequisites and command

Use PowerShell 7.2+, Git, an Azure CLI login (`az login`), and the
`containerapp` extension (`az extension add --name containerapp --upgrade`).
The selected subscription needs the Container Apps, Container Registry,
Managed Identity, and Log Analytics providers registered. The operator needs
permission to create/delete the resources, run ACR builds, query logs, and
assign `AcrPull`. No local Docker installation is required.

From the repository root, with `AZURE_SUBSCRIPTION_ID` set:

```powershell
pwsh -NoProfile -File .\deploy\azure-container-apps\ha\measure.ps1 `
  -SubscriptionId $env:AZURE_SUBSCRIPTION_ID `
  -Id meas007 -Location westus2 -Measurements MEAS-007 `
  -OutDir C:\VLiteMeasurements\meas007
```

`OutDir` must be outside this checkout, absent or empty, and must not traverse
symlinks/junctions. The defaults are 0.5, 1, and 2 vCPU; memory is always twice
CPU in GiB. To select tiers, invoke from PowerShell (rather than passing an
array through `pwsh -File`):

```powershell
& .\deploy\azure-container-apps\ha\measure.ps1 `
  -SubscriptionId $env:AZURE_SUBSCRIPTION_ID -Id custom007 `
  -Tiers 0.25,0.5,1.25 -Records 1000 -Delta 30 -Dimensions 8 `
  -OutDir C:\VLiteMeasurements\custom007
```

Allowed CPU values are 0.25..2 in 0.25 increments. Optional inputs are plain
decimal integers: records 1..2,000,000 (default 110,000), delta 1..100,000
(300), and dimensions 1..1,024 (64). Records times dimensions must not exceed
32,000,000; `2 * floor(delta / 3) + 2` must not exceed records.
These are probe safety bounds, **not qualified tenant limits**.

Offline preview (no Azure calls; specifying the revision also avoids Git):

```powershell
& .\deploy\azure-container-apps\ha\measure.ps1 -Plan `
  -SourceRevision ('a' * 40) -Id preview007 `
  -OutDir C:\VLiteMeasurements\preview007
```

The preview prints ordered Azure CLI argument arrays and returns step objects.
`<subscription>`, `<principal-id>`, `<target-b64>`, `<execution>`, and `<workspace-id>` are
placeholders resolved at execution. Poll steps repeat; `QueryLogs` is a
conditional fallback. Preview provenance assumes clean and does not attest
the working tree. A real run always checks it.

Source revision defaults to `git rev-parse HEAD`; overrides must be 40
lowercase hexadecimal characters. Dirty trees are refused unless
`-AllowDirty` is explicit. Provenance uses newline-separated, unmodified
`git status --porcelain --untracked-files=all` entries; **clean is an empty
string**, not `[]` or the word `clean`. Both provenance environment variables
are set together. The image retains Rust because the probe invokes `rustc`.
The wrapper compresses its unchanged JSON and emits numbered base64 chunks
with a SHA-256 and compressed byte count.

`-Image registry/repository:tag` skips compilation and imports that existing
measurement image into the temporary ACR. This keeps all managed-identity
pull grants within the disposable group. The source must allow ACR import
without supplied credentials and implement the same entrypoint protocol;
use an immutable digest for reproducibility. No registry passwords are used.

## MEAS-002: reader staleness under write load

```powershell
& .\deploy\azure-container-apps\ha\measure.ps1 `
  -SubscriptionId $env:AZURE_SUBSCRIPTION_ID -Id meas002 `
  -Measurements MEAS-002 -DurationSecs 360 -Rate 20 `
  -ArchiveIntervalSecs 60 -StalenessBoundMs 300000 `
  -ReaderPollMs 1000 -ObserveMs 100 `
  -OutDir C:\VLiteMeasurements\meas002
```

MEAS-002 defaults to one 1 vCPU / 2 GiB job; explicit `-Tiers` follows the
same CPU/memory rules as MEAS-007. Its optional plain-decimal inputs are:

| Option | Range | Default |
|---|---|---|
| `DurationSecs` | 60..3600 | 360 |
| `Rate` (commits/second) | 1..200 | 20 |
| `ArchiveIntervalSecs` | 60..86400 | 60 |
| `StalenessBoundMs` | 1000..3600000 | 300000 |
| `ReaderPollMs` | 100..60000, strictly below the effective staleness bound | 1000 |
| `ObserveMs` | 50..5000 | 100 |

The effective options must also satisfy
`600 + duration + 2 * (2 * archive + 120) <= 3600` seconds to fit the
replica timeout; this budget further restricts duration and archive interval.
These options are refused with MEAS-007; `Records`, `Delta`, and `Dimensions`
are refused with MEAS-002. Omitted options use the driver's defaults.

In addition to the shared resources, this mode creates a temporary
Standard_LRS StorageV2 account (`vlitemeas<id>st`) and `archive` and
`coordination` containers via the ARM control plane. Register the
`Microsoft.Storage` provider first. The operator needs role-assignment
rights for **Storage Blob Data Contributor** as well as `AcrPull`. Shared
keys and public Blob access are disabled; the job uses its user-assigned
managed identity. Its target JSON is passed as base64 to avoid Windows
`az.cmd` quoting issues; base64 is transport encoding, not encryption.
Plans retain `<target-b64>` rather than the identity client ID.

The driver runs a writer and reader, measuring acknowledged commits becoming
visible and archive-head publication under load. `MEAS-002-c1.json` (or
the corresponding explicit tier suffix) contains its original evidence.
`summary.json` records `measurement: MEAS-002`; it never includes the
subscription ID, identity client ID, or account keys. Blob capacity and
transaction charges apply in addition to the shared ACR, job, and logging
costs. The account and containers are deleted with the group under the same
cleanup policy below.

**This is evidence for a staleness proposal, not an SLO or HA qualification.**

## MEAS-006: writer-fence costs on Blob coordination

```powershell
& .\deploy\azure-container-apps\ha\measure.ps1 `
  -SubscriptionId $env:AZURE_SUBSCRIPTION_ID -Id meas006 `
  -Measurements MEAS-006 -Samples 100 -Repetitions 20 -IntervalMs 50 `
  -OutDir C:\VLiteMeasurements\meas006
```

MEAS-006 always runs one 1 vCPU / 2 GiB job from **clean source**:
`-AllowDirty`, `-KeepOnFailure` and any other tier are refused, and an
`-Image` must be pinned by `@sha256:` digest. The MEAS-007/MEAS-002 options
are refused. Its optional plain-decimal inputs are:

| Option | Range | Default |
|---|---|---|
| `Samples` (per gate metric) | 1..2000 | 100 |
| `Repetitions` (transfers) | 1..100 | 20 |
| `IntervalMs` (fence recheck) | 5..5000 | 50 |

The run refuses an existing registry or storage account name. It creates a
Standard_ZRS StorageV2 account (`vlitemeas<id>st`, shared keys and public
access disabled) with one `coordination` container, and grants the job's
identity **Storage Blob Data Contributor** on that container only. Before
measuring, the job waits up to 900 seconds for identity and role
propagation by reading an absent `meas006/preflight-*` blob; it never writes
during the preflight.

`MEAS-006-c1.json` holds one `vlite-meas006/1` document. The harness checks
its exact schema, `aca-blob` scope, clean pinned source revision, requested
settings and sample counts. It also refuses any document containing private
identifiers. Its metrics are `verify_before_commit`,
`verify_before_ack` and `commit_then_ack` (microseconds, `Samples` each), and
`transfer_to_observed_fenced` (milliseconds, `Repetitions`). Any failure
reports only `MEAS-006 failed at <step>; diagnostics withheld.` and still
requests group deletion under the cleanup policy below.

Each MEAS-006 result records `executionStatus` separately from `status`. It
is the validated terminal ACA execution status, or null if polling did not
complete. Evidence collection or cleanup failure cannot overwrite it.
`Succeeded` here does not mean the evidence was collected or accepted.
Older summaries lack this field; their result `status` may have been changed
to `Failed` during evidence collection, so it cannot recover the original
execution outcome.

The MEAS-006 `summary.json` always has a `failureDiagnostic` field. It is
null on success and after a failure at any step other than `ReadLogs`. If
`ReadLogs` fails, the field holds a few fixed codes. It never holds stderr,
console or Log Analytics lines, exception text, or any client, principal,
workspace, subscription or resource identifier:

| Field | Values |
|---|---|
| `category` | `JobFailed` (the job printed `VLITE-MEAS FAILED <code>`), `EvidenceUnavailable` (logs could not be read or were incomplete), `EvidenceIntegrity` (the framing, length, SHA-256, base64 or gzip/JSON check failed), `EvidenceRejected` (the document failed MEAS-006 validation), `JobNotSucceeded` (valid evidence, but the execution was not `Succeeded`), `Unclassified` |
| `reason` | A fixed code. `FailedMarker`; `RetrievalFailed`, `ResponseShape`, `MissingBegin`, `NeverStarted`, `StartedNoResult`, `MissingEnd` or `ChunkGap`; `DuplicateMarker`, `ChunkLength`, `Base64`, `LengthMismatch`, `ChecksumMismatch` or `PayloadDecode`; `Schema`, `PrivateIdentifier`, `Provenance`, `RequestedSettings`, `Metrics`, `Limits` or `Unclassified`; `ExecutionNotSucceeded` |
| `jobExitCode` | The numeric exit code from the `FAILED` marker (0..255), otherwise null |
| `consoleEvidence`, `logAnalyticsEvidence` | `NotQueried`, `Unavailable`, `Incomplete`, `IntegrityFailure`, `JobFailed` or `Verified`, for each source |
| `logAnalyticsAttempts` | The number of Log Analytics queries made (0..72) |
| `executionStatus` | The validated terminal execution status (as in the result), or null |
| `jobStarted` | `true` if any source showed a wrapper liveness marker, `false` if lines were read but none was a marker, null if no line was read |
| `lastStage` | The furthest `VLITE-MEAS STAGE` seen: `6-COLLECT` (collector started) or `6-ENCODE` (collector exited 0), otherwise null |
| `lastAliveSecs` | The largest `VLITE-MEAS ALIVE <seconds>` heartbeat seen, otherwise null |
| `signal` | The number from `VLITE-MEAS SIGNAL <n>` (the wrapper caught HUP, INT or TERM), otherwise null |
| `lateQueryAttempts`, `lateLogAnalyticsEvidence`, `lateJobExitCode` | The diagnostic-only late re-query described below: attempts (0..3), outcome (`NotQueried`, then the `logAnalyticsEvidence` codes) and any `FAILED` exit code |

These codes help an operator with diagnosis only. They are not measurement
results and do not qualify the run.

For `EvidenceUnavailable` / `MissingBegin`, `reason` is split once any line
was read: `NeverStarted` means no wrapper marker was seen (the container did
not start the wrapper, or the logs were not ingested, or the image predates
the markers); `StartedNoResult` means the wrapper started but no complete
payload was recovered. `MissingBegin` remains when no line at all was read.
The wrapper prints `VLITE-MEAS START <kind>` at once, and for MEAS-006 prints
`VLITE-MEAS STAGE 6-COLLECT` before the collector, `VLITE-MEAS ALIVE
<seconds>` every 60 s while it runs and `VLITE-MEAS STAGE 6-ENCODE` after it
exits 0. It prints `VLITE-MEAS SIGNAL <n>` if terminated by a catchable
signal; an OOM kill or SIGKILL prints nothing further, so the last heartbeat
bounds when the container died. Markers carry only fixed codes and numbers.
They are ignored by payload framing; the collector stderr stays suppressed.

When MEAS-006 fails at `ReadLogs` with `EvidenceUnavailable`, the harness
re-queries Log Analytics up to three times, 120 s apart (about 6 minutes),
before deleting the group, to tell ingestion lag from missing output. This
re-query only updates the diagnostic fields above. Its payload is discarded
and never qualifies the run, even if `Verified`. `-KeepOnFailure` remains
refused for MEAS-006.

**This is proposal evidence for fencing overhead, not an SLO, a stale-ack
window, or HA qualification.**

## MEAS-004/005 gateway orchestration (pilot failed; no measurement evidence)

`-Measurements MEAS-004` or `MEAS-005` dispatches to `measure-gateway.ps1`.
`-Plan` works on any host. It prints every step with placeholders
(`<subscription>`, `<principal-id:ROLE>`, `<projections:ROLE>`, digests) and
returns them. It writes no files and runs no process. A live run requires a
Linux/WSL operator host, `-FixtureServer` (absolute path of a native
`vlite-server` built from the same source revision; the operator must
ensure this) and, for MEAS-005, `-ControlProgram` (the native `ha-meas005`).
`-GatewayProfile pilot|full` (default `pilot`) selects the contract's
settings. `Tiers`, `Records`, `Delta`, `Dimensions`, `Image` and
`-KeepOnFailure` are refused.

The planned live order is:

1. Check that the uniquely named, tagged group and the registry and storage
   names are unused. Check that `/measurement` exists and is empty.
2. Create the group, ACR, `gateway-measure` image, one identity per role
   (driver, gateway, writer, reader), `AcrPull`, the storage account with
   control containers (only the driver may write), and the environment.
3. Run `measure-gateway-fixture.py prepare` **once**. Verify its single-line
   summary and each export exactly. The checks cover:
   - paths;
   - digests;
   - role file allowances (backends never receive signing, admin or driver
     files);
   - generation and source;
   - one shared lineage and fixture digest;
   - the planned build, which must equal the harness's build command;
   - `executed: []`;
   - a credential deadline inside the bootstrap window.

   Any mismatch stops the run before the build. The emptiness of
   `/measurement` is checked again just before `prepare`. Only one run may
   use the host's `/measurement` at a time; there is no lock.
4. Build the planned fixture image. Create the publish job and start it once,
   with no retry. The run continues only on an exact publish receipt.
5. Create writer (and reader for MEAS-005) and two gateway replicas, plus the
   driver job. Each gets only its own role's projections, a role-specific
   control target and the exact fixture image digest. Remove the local
   fixture with `cleanup` right after distribution.
6. Record sorted app/revision/replica/container identities; missing or invalid
   restart counts, duplicate or foreign identities refuse the run. Any
   restart refuses it too. Start the driver once, verify its evidence, and
   record inventory again. Changed identities mark the run `Incomplete`;
   ordering changes and observers' intentional final shutdown do not.
7. MEAS-005 only: wait for the bounded observer window, then collect one
   observer report per recorded replica. Run the offline analysis when every
   report is present.
8. Always remove local fixture files, delete the group and write
   `summary.json`.

The 2-hour credential lifetime is a hard limit. Before each create and start
step, the run checks the time left: 2 minutes for most steps, the full
1-hour job timeout plus margin before the driver starts, and the observer
window plus 15 minutes before MEAS-005 apps start. If time is short, the run
fails with `CredentialWindow`. Errors are static
(`<MEAS> failed at <step>; diagnostics withheld.`). The summary records a
fixed `failureReason` code and never includes identifiers or secrets.

MEAS-005 also checks remaining observer lifetime immediately before mutation.
Its conservative start is recorded before creating the first observer app,
not when all apps eventually become ready. The remaining window must cover
the native worst-case driver budget:
`mutations * (observation_deadline_ms + poll_interval_ms + 138000)`.
Otherwise `ObserverWindow` stops the run before the driver starts. Full
profiles have little setup slack and may refuse even with valid credentials;
this does not justify shortening the budget or extending a qualification gate.

MEAS-005's pilot now uses the contract's 100 ms poll, one sacrificial
revocation, three mutations and 1,000 ms observation deadline. Full profiles
use ten revocations, twenty mutations and a 30,000 ms deadline. The
MEAS-005-only `-GatewayPollMs` accepts an integer from 1 through 5,000; its
default is 100 for `pilot` and 1,000 for `full`. The proposed full sweep uses
100, 250, 500, 1,000, 2,000 and 5,000 ms, each in a separate, isolated
fresh run. It is not an automatic retry or sweep. Fixture preparation
forwards and verifies the poll, replica cap and revocation count before
export; gateway and both backend polls must match the mutator. MEAS-004
retains its existing 1,000 ms poll and ten revocations.

Clock calibration is still not implemented. MEAS-005 summaries carry
`ClockCalibrationUnavailable` and stay `Incomplete`. Sampling observers alone
does not calibrate the actual mutator job, and before/after agreement does
not establish a run-wide ACA clock-motion bound. No zero offsets, operator
UTC or assumed NTP precision substitute for that evidence. Clock reports
now refuse noninteger/negative UTC values, reversed timestamps, elapsed
values outside the one-minute sample bound, and wall/monotonic disagreement
above the existing 10 ms limit. These checks do not supply calibration.
Observer evidence must match its recorded app and revision, and the
analyzer requires conservative lifetime through the entire command plus
observation deadline, not merely through an early matching observation.

The actual mutating driver execution now also prints a job-local
`VLITE-CLOCK` companion (`vlite-meas005-driver-clock/1`) ahead of its
unchanged mutation frame. It samples only that container's own wall,
monotonic and, where present, raw clocks around the single native attempt,
and binds them to the ACA job execution, replica, boot id and the exact
mutation evidence digest. The harness checks it offline with
`check_driver_clock` and saves `MEAS-005-driver-clock.json` and
`MEAS-005-driver-clock-check.json`. The reasons it can add are
`DriverClockContinuityUnavailable`, `DriverClockEvidenceRejected` and
`DriverClockContinuityIncomplete`. These are advisory: they never fail the
run or retry the driver. A complete check says the sampled points were
continuous and within the existing 10 ms allowance for this execution. It
does not bound unsampled motion, calibrate any observer, or populate offset,
uncertainty, excursion or calibration values. Those remain null, and
`ClockCalibrationUnavailable` still applies.

The first live MEAS-004 attempt failed at `BuildMeasureImage` with
`failureReason: Unclassified`; no fixture digest, publish execution, driver
execution or evidence was produced. Cleanup reported `Deleted`, and a later
read-only check confirmed the group absent. The likely incompatibility was
using `az acr build` for a Dockerfile that uses BuildKit cache mounts; the
replacement ACR task explicitly enables BuildKit. On 2026-11-21 the build-only
ACR diagnostic `cc1` succeeded from clean `1fa98c5c8702a74c065cfc9c99f1b055327a8b7b`.
The revision tag resolved to `sha256:79d5b66fd43a21257e51dbfdd6abefd3762b46ca84cf8ae99780618daab1b3b9`
before the temporary group was confirmed absent. This verifies the replacement
build path, not the cause of the earlier failure. No fixture, ACA app or job
was created by the diagnostic.

The isolated MEAS-004 pilot `m496673cd9` then ran from that clean revision
with a matching native server. It passed the measurement-image build and
fixture validation but failed at `BuildFixtureImage` with `Unclassified`.
There was no publish or driver execution and no measurement evidence. The
fixture was removed. The cleanup wait expired at `WaitDeleted`; a read-only
check at 2026-11-21T04:32:13Z still found the owned group `Deleting`, with only
the managed-environment resource listed. The registry had already been
deleted, so its build logs were unavailable. Preserve the failed summary
and postflight; do not restart the pilot or overlap its ongoing deletion.
Next: confirm group absence and diagnose the fixture-image failure before
another MEAS-004 attempt. MEAS-005 has not run. MEAS-006's collector now uses
LF, but the fix has not been verified in ACA.

Follow-up at 2026-11-21T04:52:36Z confirmed the pilot group absent; Azure
recorded deletion succeeding at 04:34:00Z. The original failed summary and
postflight remain unchanged. A local Docker reproduction then confirmed that
the generated fixture layer's `COPY --chmod=0400` requires BuildKit, while the
fixture step uses classic `az acr build`. Unlike the base-image build, this
layer needs no BuildKit feature: it now uses `COPY --chown=10001:10001` followed
by `RUN chmod 0400`. A real classic-builder smoke check using a cached
non-root base verified the unchanged fixture digest, UID/GID 10001 and mode
0400. This fixes a demonstrated build incompatibility; it does not recover
the deleted Azure logs or constitute a successful ACA pilot. MEAS-006 run
`m6lf21a` has started from clean `1fa98c5c` with an LF-only collector. Live
campaigns remain serialized; MEAS-004 must wait for that run and cleanup.

Superseding MEAS-006 result: `m6lf21a` ended at 2026-11-21T05:33:04Z with
`ReadLogs` / `EvidenceUnavailable` / `MissingBegin`. Console retrieval was
unavailable and all twelve Log Analytics attempts lacked complete evidence.
No exit code or measurement document was recovered. Unlike the earlier
exit-2 failure, this does not establish whether the collector ran or why the
job produced no retrievable evidence. The old result `status` can be changed
to `Failed` by evidence collection itself; the harness now preserves the
validated `executionStatus` independently for future runs.

Root cause is unproven. The old wrapper printed nothing until the collector
exited and suppressed MEAS-006 stderr, so a killed container (2Gi OOM, the
3600 s replica timeout, eviction or a start failure), a hung collector and
late Log Analytics ingestion all looked the same. The twelve 10 s attempts
(about 2 minutes) were also shorter than typical ACA ingestion delay; the
harness now makes 72. The wrapper liveness markers, `jobStarted`,
`lastStage`, `lastAliveSecs`, `signal`, `executionStatus` in the diagnostic
and the late re-query are meant to separate these causes. MEAS-006 remains
unqualified pending one approved throwaway rerun with a freshly built image.

The deletion wait also expired. A separate read-only check confirmed the
group absent at 2026-11-21T05:48:14Z; `cleanup-followup.json` binds that
confirmation to the unchanged failed summary and postflight by SHA-256.
Deletion was not resubmitted. MEAS-006 remains unqualified and must not be
blindly rerun. The separate MEAS-004 fixture-repair pilot may proceed after
its own clean-source, ownership, empty-fixture and zero-live-group preflight.

The subsequent MEAS-004 pilot `m4fix21b`, from clean `5f080c61`, stopped at
`ValidateFixture` / `FixtureLineage`. It never reached the fixture-image build,
publish or driver, and produced no measurement evidence. The private fixture
was removed; the Azure deletion wait expired, so the original failed summary
and postflight remain intact while a read-only observer checks the existing
deletion. No deletion or campaign was resubmitted.

A local reproduction using that same native binary and fixture generator
isolated a signed/unsigned validation defect: database lineages are nonzero
`u64`, but the PowerShell validator accepted only signed integers and cast to
`long`. JSON integers above `Int64.MaxValue` arrive as `BigInteger` and were
incorrectly refused. Validation now accepts the exact nonzero `u64` range,
returns `ulong`, and still rejects mismatched role lineages, including adjacent
values near `UInt64.MaxValue`. Boundary and invalid-type regressions pass on
Windows and the pilot's Linux PowerShell runtime. The real generated fixture
fails the old validator with `FixtureLineage` and passes the repaired full
summary validation without any Azure calls; its private files were removed.
The original pilot's precise lineage was not retained, so this reproduces and
fixes a concrete cause of that refusal rather than recovering its deleted
private metadata. ACA fixture-image and workload validation remain pending.

An independent read-only check confirmed `m4fix21b` absent at
`2026-11-21T06:29:04Z`, after the bounded observer expired. A separate cleanup
follow-up binds the unchanged failed summary and postflight by SHA-256; deletion
was not resubmitted. Clean `537c74d1` then passed a matching-source native build
and real fixture/full-summary preflight with no Azure calls and an empty private
fixture root. A single fresh pilot, `m4u64a`, started at `06:31:35Z` after account,
source, fixture, prior-cleanup and zero-tagged-group checks.

That pilot passed fixture validation, so the lineage repair worked live. The
publish job then reached `Succeeded` by 06:45:04Z, but the run failed at
`ReadPublish` (`failureReason: PublishEvidence`) before the driver started:
the console read was unavailable and no Log Analytics rows were usable within
the 12-attempt (about 2 minute) wait. The summary has empty `evidence` and
`measurementStatus: Failed`. The fixture was removed. The group's deletion wait
expired (`cleanupStatus: Failed` at `WaitDeleted`) while Azure was still
deleting it; the failed summary and postflight stay unchanged and deletion is
not resubmitted. No measurement evidence exists, and this run is not
qualification evidence.

This was the fourth live run to stop at an evidence read. Microsoft's ACA
documentation says to expect a delay of several minutes before console logs
are queryable, and a Q&A thread reports about 5 to 9.5 minutes (p95). The
collector therefore now waits up to 72 attempts (about 12 minutes). Gateway
group deletion only starts after the read gives up, so the group stays alive
while it waits. One later live run, `m4log72`, read its evidence within this
wait (see below). That is a single observation, so the ingestion-delay
explanation is supported but not proven; a Log Analytics access or retention
cause has not been ruled out. When a gateway read fails, `summary.json` now also carries a
`failureDiagnostic` with the same static codes as MEAS-006 (`category`,
`reason`, `jobExitCode`, `consoleEvidence`, `logAnalyticsEvidence` and
`logAnalyticsAttempts`). It is null on success and never holds native error
text, log lines or identifiers. These codes help an operator with diagnosis
only and are not measurement results.

The following are unverified until a live run:

- ACA secret-volume behavior and limits;
- internal HTTPS trust;
- empty environment values;
- console log `--tail` limits;
- probe behavior;
- `--yaml` acceptance of JSON manifests;
- ACR `--target` builds;
- whether the run fits inside 2 hours.

An `AcrPull` propagation delay fails the publish step, which is never
retried.

Pilot `m4log72` (clean `479ede2a`, started `07:16:21Z`, one fresh group after
the zero-tagged-group check) is the first live MEAS-004 run to finish:
`status: Succeeded`, `measurementStatus: Collected`, no failed step. Both job
reads recovered through the Log Analytics fallback and the evidence file was
written about 22 minutes after launch. The record is
`proposal_evidence_only`, profile `pilot`: 5 requests per window at 5 rps, 1 s
windows, two direct and two gateway windows in ABBA order, all 200 with no
errors or mismatches. Paired window mean deltas (about 4.9 and 7.9 ms) are not a
per-request overhead distribution, and CPU entitlement, pure CPU overhead and
scale-out remain unknown. The replica inventory was identical before and after
(2 gateway, 1 writer, 0 restarts). The harness cleanup ended `NeedsVerification`
at `WaitDeleted` because Azure was still deleting the group; a separate
hash-bound follow-up later confirmed it absent with the original summary and
postflight unchanged and no deletion resubmitted.

The later full-profile run `m4full24a` (clean `c37e39f6`) collected all
25 windows; publisher and driver succeeded. Of 2,076,000 scheduled requests,
868,672 started and succeeded; 1,207,328 were skipped as late, none for capacity.
The 800/1600-RPS loads were therefore not achieved. Cleanup again exceeded the
harness wait; a separate hash-bound read-only lookup confirmed the group absent
at `2026-11-24T10:14:03.7890066Z`, preserving the original failure records.
Full provenance and immutable hashes are recorded in the
[tenant-provisioning PRD](../../../docs/projects/feature-aca-ha/design-tenant-provisioning/design-tenant-provisioning.prd.md#isolated-azure-qualification-preflight-2026-11-24).

A subsequent Linux-only local reproduction exposed a driver limitation:
Tokio's millisecond timer wakes too late for many high-rate slots even with no
HTTP work. Pacing now uses the dedicated caller's OS thread sleep, leaving
async HTTP on runtime workers and preserving the one-period late-drop policy.
The corrected driver improved dispatch in a bounded mock-HTTP comparison,
but that is generator evidence only, not a repeat Azure run or capacity proof.
The original campaign remains unchanged and cannot qualify the corrected code.

**No MEAS-005 evidence exists. Neither the pilot, the full-profile collection
nor the local generator check qualifies serving, SLOs or gates.**

## Outputs, cost, and cleanup

`MEAS-007-c05.json`, `MEAS-007-c1.json`, and `MEAS-007-c2.json` contain the
probe's original JSON bytes, after chunk sequencing, compressed length,
SHA-256, gzip, UTF-8, and JSON-object verification. Custom tiers remove the
decimal point (`0.25` becomes `c025`). `summary.json` records requested CPU
and memory, execution name, status, compressed SHA-256, file name, source
revision, dirty flag, and cleanup status, **never the subscription ID**.
Failure summaries retain completed results. No JSON fields from the probe
are interpreted or treated as a serving/SLO qualification.
The recorded SHA-256 covers the compressed log payload, not the decompressed
JSON file. Use a separate file checksum when preserving the extracted files;
do not compare `Get-FileHash` on those files with the transport checksum.

`failedStep` identifies the failed measurement/setup step (for example,
`BuildImage`); `cleanupFailedStep` independently identifies `DeleteGroup` or
`WaitDeleted` if cleanup fails. Both are null when that part of the run
succeeds. Only plan step names are saved, not native error text or command
arguments; cleanup does not overwrite the original failure step.

There are charges for Basic ACR, remote builds, job CPU/memory, and log
ingestion. Builds are bounded at 7,200 seconds, replicas at 3,600 seconds,
execution polling at 4,200 seconds per tier, log fallback at 72 attempts
(10-second intervals, about 12 minutes), and deletion polling at 600 seconds. Job starts retry
for managed-identity role propagation. Native CLI diagnostics/build logs are
withheld to avoid exposing credentials or subscription identifiers.

The `finally` block **always requests resource-group deletion and waits up to
600 seconds for confirmation**, including after measurement failure. The only explicit exception
is `-KeepOnFailure`, which retains a failed run and warns with its group name.
Cleanup failure is loud and fails the command. Forced process termination,
machine loss, or revoked permissions can still prevent cleanup. The expiry
tag is a cleanup aid, **not an automatic deletion service**.

A `WaitDeleted` timeout can occur while Azure continues an accepted deletion.
Check the exact run's group and ownership tags before acting. If the group is
already `Deleting`, observe that operation rather than rerunning the measurements
or repeatedly requesting deletion. Confirm absence with `az group exists`,
checking the command's exit code and its boolean result; a lookup error is not
proof of deletion. Preserve the original failed summary and record any later
cleanup confirmation separately with its timestamp. Successful jobs and later
cleanup do not retroactively make the original command successful or qualify HA.

To clean up a retained/interrupted run by owner and run-ID tags, review the
selected names and delete them (no JMESPath filters, for Windows `az.cmd`
compatibility):

```powershell
$groups = az group list --subscription $env:AZURE_SUBSCRIPTION_ID --output json |
  ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Could not list measurement groups.' }
$owned = @($groups | Where-Object {
  $_.tags.vliteMeasurementOwner -eq 'ha-harness' -and
  $_.tags.vliteMeasurement -eq 'meas007' -and
  $_.name -eq 'vlite-ha-meas-meas007'
})
$owned.name
$owned | ForEach-Object {
  az group delete --subscription $env:AZURE_SUBSCRIPTION_ID --name $_.name --yes
  if ($LASTEXITCODE -ne 0) { throw 'Measurement group cleanup failed.' }
}
```

The tags are `vliteMeasurement=<id>`,
`vliteMeasurementOwner=ha-harness`, and
`vliteMeasurementExpires=<UTC creation time + one day>`.
