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

The MEAS-006 `summary.json` always has a `failureDiagnostic` field. It is
null on success and after a failure at any step other than `ReadLogs`. If
`ReadLogs` fails, the field holds a few fixed codes. It never holds stderr,
console or Log Analytics lines, exception text, or any client, principal,
workspace, subscription or resource identifier:

| Field | Values |
|---|---|
| `category` | `JobFailed` (the job printed `VLITE-MEAS FAILED <code>`), `EvidenceUnavailable` (logs could not be read or were incomplete), `EvidenceIntegrity` (the framing, length, SHA-256, base64 or gzip/JSON check failed), `EvidenceRejected` (the document failed MEAS-006 validation), `JobNotSucceeded` (valid evidence, but the execution was not `Succeeded`), `Unclassified` |
| `reason` | A fixed code. `FailedMarker`; `RetrievalFailed`, `ResponseShape`, `MissingBegin`, `MissingEnd` or `ChunkGap`; `DuplicateMarker`, `ChunkLength`, `Base64`, `LengthMismatch`, `ChecksumMismatch` or `PayloadDecode`; `Schema`, `PrivateIdentifier`, `Provenance`, `RequestedSettings`, `Metrics`, `Limits` or `Unclassified`; `ExecutionNotSucceeded` |
| `jobExitCode` | The numeric exit code from the `FAILED` marker (0..255), otherwise null |
| `consoleEvidence`, `logAnalyticsEvidence` | `NotQueried`, `Unavailable`, `Incomplete`, `IntegrityFailure`, `JobFailed` or `Verified`, for each source |
| `logAnalyticsAttempts` | The number of Log Analytics queries made (0..12) |

These codes help an operator with diagnosis only. They are not measurement
results and do not qualify the run.

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
6. Record replica inventory; any restart refuses the run. Start the driver
   once, verify its evidence, and record inventory again. A changed
   inventory marks the run `Incomplete`.
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

Clock calibration is not implemented. MEAS-005 summaries therefore always
carry `ClockCalibrationUnavailable` and stay `Incomplete`. The fixed fixture
bootstraps with a 1,000 ms poll, so the contract's 100 ms pilot setting and
poll sweep cannot be run.

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
retried. **No MEAS-004/005 evidence exists. Nothing here qualifies serving,
SLOs or gates.**

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
execution polling at 4,200 seconds per tier, log fallback at 12 attempts
(10-second intervals), and deletion polling at 600 seconds. Job starts retry
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
