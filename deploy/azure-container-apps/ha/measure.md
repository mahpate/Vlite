# Temporary ACA measurement jobs

MEAS-007 packages the existing reader-apply resource probe for the
owner-approved **separate, temporary measurement apps**. It creates a new,
tagged resource group containing an ACR, user-assigned identity, Consumption
environment (with Log Analytics), and one Manual job per requested CPU tier.
It never reuses a group or touches the live app, environment, or share.
**MEAS-001..006 are not implemented yet** and are refused.

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
`<subscription>`, `<principal-id>`, `<execution>`, and `<workspace-id>` are
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
