# Temporary ACA measurement jobs

MEAS-007 packages the existing reader-apply resource probe for the
owner-approved **separate, temporary measurement apps**. It creates a new,
tagged resource group containing an ACR, user-assigned identity, Consumption
environment (with Log Analytics), and one Manual job per requested CPU tier.
It never reuses a group or touches the live app, environment, or share.
MEAS-002 also measures reader staleness under write load against real Blob
Storage, and MEAS-006 measures writer-fence costs against a temporary Blob
coordination container. **MEAS-001 and MEAS-003..005 are still refused**, as
are combinations of measurement IDs. MEAS-004/005 have a gateway container
runtime (`measure-gateway-entrypoint.py`, packaged with `python3` in the
`gateway-measure` image) and a contract (`measure-gateway-contract.md`). The
offline Linux/WSL exact-fixture preparation (`measure-gateway-fixture.py`) is
also in place. It bootstraps once at `/measurement/bootstrap`. It then exports
and verifies the four role bundles and writes a private image context for that
bootstrap's own `database.vlite`. It only plans the `az acr build`. There is
still no live orchestration harness: nothing here provisions, builds, writes
secrets for, or starts MEAS-004/005. Offline tests run in Linux/WSL:
`python3 -B measure-gateway-fixture.test.py`.

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
