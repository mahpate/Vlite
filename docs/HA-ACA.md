# Opt-in HA operations on Azure Container Apps

These are experimental operator procedures, not production qualification.
**HA is opt-in.** With none of the HA flags or files, VLite runs exactly as
the single-writer deployment does today: no new headers, fields, reads,
errors or startup requirements.

Gateway and reader scale-out add read capacity and isolation, **not writer
HA**. There is one writer. The writer serving fence is **explicitly opt-in**,
and there is no automatic failover. Failover is an
operator-run, reported and acknowledged procedure; the command prepares
authority transfer, not a running replacement.

**Azure Files remains experimental and is not power-loss qualified.**
The [singleton deployment](../deploy/azure-container-apps/README.md) is
unchanged. The separate [HA scaffolding](../deploy/azure-container-apps/ha/README.md)
stops at its qualification gate before rollout; these instructions do not
bypass it. Reader verification, fencing/manual drills, migration rollback,
and measured SLO/chaos/runbook review gates remain separate requirements
(GATE-004 through GATE-007 in the
[project plan](projects/feature-aca-ha/feature-aca-ha.prd.md)).
Older [gateway boundary tables](projects/feature-aca-ha/gateway.md) and
[local test evidence](projects/feature-aca-ha/gateway-test-plan.md) are not
proof of current writer HA or ACA qualification.

## What has been measured

- **Reader lag (MEAS-002):** A writer and reader in the same temporary ACA job
  used a 60-second archive interval. All 7,200 acknowledged commits became
  visible to the reader; the median delay was 30.7 seconds and the longest
  observed was 60.9 seconds. This was not a target deployment tier or a
  bound on reader lag.
- **Archive faults (MEAS-003):** In local tests with an in-memory object
  store, the loss report agreed with the records missing after restore.
  Those tests did not measure data loss during an ACA outage.
- **Gateway load and control-state changes (MEAS-004/005):** Neither has been
  measured on ACA. The local drivers exist, but the ACA harness still needs to
  distribute the same bootstrap database to each backend. Generating a new
  database gives it a different lineage and invalidates the comparison.
- **Writer fencing (MEAS-006):** In-memory and local Blob emulator runs
  exercised the writer checks. Both ACA attempts reached log collection
  without a usable result; the cause is unknown. Neither fencing latency on ACA nor a bound
  on how long a stale writer can acknowledge writes has been established.
- **Reader sizing (MEAS-007):** Temporary ACA jobs measured resources during
  reader apply, index rebuild and compaction on test data. These were not
  serving workloads and do not establish a supported tenant size.

These are not availability, recovery-time or data-loss guarantees. There is
no HA dashboard or automatic failover. Use the operator procedures below;
see the [measurement record](projects/feature-aca-ha/feature-aca-ha.prd.md#measurement-plans-qualification-remains-gated)
for the fixtures and limitations.

## Prepare private inputs

Use an approved isolated environment, the matching `vlite-server` binary,
verified backups and archive, scoped Blob identity permissions, and a
recorded recovery decision. Do not touch the live singleton for a drill.
Measure capacity, polling, staleness and drain inputs; there are no
qualified timing defaults or promised RPO/RTO.

Examples below use PowerShell and Windows paths. Replace bracketed
placeholders before running; paths refer to private operator files, not
repository files. In Linux containers use the corresponding mounted paths.
Configuration schemas are in the
[API reference](API.md#gateway-mode-and-gateway-trusted-backends-experimental-opt-in-off-by-default);
target JSON is the cloud backup policy's target, not the whole policy.
Keep control and tenant archive targets separate.

Use the existing **Container Apps secrets**, the same way as the admin
token; no Key Vault or other new service is required. Project only the
role's required files read-only, with generation-specific immutable secret
names. Backends receive verification keys, never signing seeds; gateways
receive no admin token. Preserve private originals and verify permissions
on the actual platform. Follow the
[private material procedure](../deploy/azure-container-apps/ha/README.md#private-material-and-intended-reconciliation).
Never put key or token values in command lines, logs, examples, control-state
objects or Git. Disable transcripts, debug tracing and payload capture.
Use file arguments and `VLITE_AUTH_TOKEN_FILE`, not literal credentials.

To inspect shipped usage without accessing Azure, run these without arguments:

```powershell
vlite-server failover
vlite-server authority
vlite-server gateway
vlite-server gateway-keygen
vlite-server control-state
vlite-server cloud-sets
vlite-server cloud-restore
```

Missing-argument errors are expected. The following usage excerpts are quoted
verbatim; examples beneath them supply the required arguments.

## Initialize and claim writer authority

Initialize only for an operator-verified local database. After failover,
startup restores the authority's verified archived base before opening the
database, including when the local file is stale, ahead, or absent. Read the database's
16-character lowercase hexadecimal `lineage` from
`GET /v1/admin/serve-status` on that standalone server (a gateway-trusted
backend has no admin path); files without a v4 lineage are refused.
Stop that server before starting the authority-enabled writer.

```text
vlite-server authority init|show --target-file <json> [--shared-key-file <path>] [init only, all required: --installation-id <id> --scope-id <id> --lineage <16 lowercase hex>]
```

```powershell
vlite-server authority init --target-file C:\private\authority-target.json `
  --installation-id <installation-id> --scope-id <scope-id> --lineage <lineage>
vlite-server authority show --target-file C:\private\authority-target.json
vlite-server C:\private\database.vlite --serve-mode writer `
  --token-file C:\private\admin-token.txt `
  --authority-target-file C:\private\authority-target.json `
  --authority-recheck-ms 5000
```

IDs use 1-128 ASCII letters, digits, `.`, `_` or `-`. Initialization creates
the authority once (`writer_authority_exists` otherwise); show never mutates
it. Both print schema, installation/scope IDs, lineage, epoch, revision,
phase, `writer_present`, base and head as JSON, never the full writer ID.
For Azurite add `--shared-key-file <path>` to authority commands and
`--authority-shared-key-file <path>` to server startup. Authority options
have no environment-variable or ambient backup-key fallback.

Before binding a listener, startup reads authority, restores any archived
base in private staging, checks the lineage and sequence, and CAS-claims the
prepared epoch using that same authority observation. Missing, invalid, unavailable or unresolved
authority fails closed. A lineage mismatch returns
`writer_authority_lineage_mismatch`; an existing writer returns
`writer_authority_claimed`. A base verification mismatch (set, manifest,
installation/scope, lineage, epoch, index/chain or sequence) returns HTTP 409
`writer_authority_base_mismatch` without replacing the local file.
Unsupported recovery configurations return HTTP 409
`writer_authority_base_unverified` before downloading: encryption, nonpersistent
durability, a managed-compaction selected candidate, or recovery mode.
The CLI rejects `--durability none` before authority access.
A changed authority or lost claim CAS returns
`writer_authority_conflict`. No automatic reclaim or election occurs.
Even a restart requires an explicit, acknowledged operator failover.

Authority requires `--serve-mode writer` and persistent durability; gRPC is
refused because it has no response barrier. `--gateway-trust-file` remains
compatible. `--authority-recheck-ms` is 500-60000 (default 5000), only with
a target file. The writer verifies authority around writes and periodically
while idle; an observed replacement fences it, drains it and exits with code
1. A pre-commit authority outage fails readiness/writes closed but can recover
on a fresh periodic check without a write. Recheck timing is not a guaranteed
wall-clock zombie bound: store latency and process pauses still matter.
Without authority options there is no store access, recheck task or fence.

With a claimed authority, cloud-backup snapshots publish HA manifests and
advance the authority head by CAS; continuous archive checkpoints extend that
head through immutable HA indexes, never the legacy archive. Every archive
commit rechecks the writer epoch. `failover report` therefore reflects the
authority-bound archived sequence, not merely local backup status.
Retention never deletes the set named by the authority head or base. In a
cycle where the authority cannot be read it condemns no Published set; it only
finishes deletes already in progress and removes stale staging sets.

For an **admin-directory writer**, configure cloud backup with
`PUT /v1/admin/cloud-backup` before initializing authority:
`authority init --installation-id` must use the `installation_id` shown by
`GET /v1/admin/cloud-backup`.

For a **gateway-trust writer**, `VLITE_ADMIN_DIR` is prohibited. Use the
writer-only `--archive-policy-file <json>` together with
`--serve-mode writer --authority-target-file <json>`. There is no policy
environment variable or additional REST configuration route. The file is the
existing cloud-backup policy JSON, capped at 64 KiB; unknown fields, invalid
targets, out-of-range settings, `enabled:false`, and missing/disabled
`archive.enabled` refuse startup before listening with
`invalid_arguments: archive_policy_invalid`. It cannot be combined with an
admin directory: admin-directory writers must keep using the REST path.

The exact gateway-trust startup order is:

1. Prepare the durable v4/v5 database and record its lineage, then stop the
   preparatory server. Choose a **16-character lowercase hexadecimal**
   installation id (the backup runtime's format) and a scope id.
2. Run `authority init --target-file <archive-target.json>
   --installation-id <chosen-id> --scope-id <scope> --lineage <lineage>`.
   Authority initialization comes **before** the policy-enabled writer;
   neither its startup nor the policy file creates authority.
3. Mount the matching archive target and enabled policy JSON (in ACA,
   `/run/vlite/archive-target.json` and `/run/vlite/archive-policy.json`),
   then start the writer with both file flags and `--gateway-trust-file`.
   Startup validates the policy, restores/claims authority, and opens private
   state in `<database path>.cloud-backup/` (owner-only on Unix). It seeds
   `installation-id` from the authority record before configuring the policy;
   an existing different id refuses with
   `writer_authority_installation_mismatch`.
4. After router construction, startup attempts one initial snapshot on a
   blocking worker if the runtime has no published set, even when the regular
   snapshot interval is days. Success/failure is logged to stderr; failure
   does not stop serving, remains visible in cloud-backup status, and is
   retried by scheduling. An existing published set skips this initial run.
   Restarts still require the explicit failover above; the next scheduled
   archive tick publishes a new epoch's base before extending its archive.

For example, the policy (target fields must match the mounted authority target):

```json
{
  "target": {
    "account": "exampleaccount",
    "container": "archive-data",
    "coordination_container": "archive-coordination",
    "prefix": "tenant/",
    "auth": "managed_identity",
    "client_id": "<writer-managed-identity-client-id>"
  },
  "enabled": true,
  "interval_secs": 3600,
  "archive": { "enabled": true, "interval_secs": 300 }
}
```

In either configuration path, the backup target's account, container,
coordination container, prefix and endpoint must match the authority target.
Authentication mode and managed-identity client id are not part of that comparison.
Policy updates and each publish/archive refuse mismatches with HTTP 409
`writer_authority_target_mismatch` or
`writer_authority_installation_mismatch`, before uploading anything.
A concurrent head change returns HTTP 409 `writer_rebase_required`; the next
cycle takes a new snapshot rather than retrying the stale head observation.
Without authority, snapshot and checkpoint publication remain unchanged.

## Planned or unplanned writer failover

**When:** preparing a manual handoff after maintenance or writer loss in an
approved isolated exercise. **Prerequisites:** existing valid authority,
access to its coordination target, archive verification, exclusive operator
control, and infrastructure proof that the old writer cannot run.

For planned work, first stop client writes, quiesce/flush the writer using
the existing shutdown procedure, and capture its final observed sequence/time
and archive status if available. For unplanned loss, missing observations
mean unknown loss, not zero loss.

Deactivate **all** old writer revisions, wait for **every** revision to have
zero replicas, and re-enumerate to confirm they remain stopped. Follow the
[deployment stop requirements](../deploy/azure-container-apps/ha/README.md#private-material-and-intended-reconciliation).
Do not treat one replica, single-revision mode, a dead process, or an expired
lease as fencing. Never acknowledge zero based only on app scale settings.

Exact usage:

```text
vlite-server failover report|execute --target-file <json> [--shared-key-file <path>] [--archive-status-file <json>] [--observed-sequence <n> --observed-unix-ms <ms>] [execute only, all required: --ack-report-id <id> --expected-epoch <n> --expected-revision <rev> --old-replicas <n>]
```

After confirming the stop, obtain and privately retain the report:

```powershell
vlite-server failover report --target-file C:\private\authority-target.json
```

Read both `report` and `text`. Check scope, lineage, epoch, revision,
archived position and disclosed loss. The authority's durable head is the
recovery boundary; a newer status checkpoint is not authority evidence.
Commits after the archived position are lost. An observation only measures
loss at that position; later loss can still be unknown.

If available, add `--archive-status-file C:\private\archive-status.json`
(the archive status object only, at most 64 KiB), and the paired
`--observed-sequence <n> --observed-unix-ms <ms>`. Use the **identical**
optional inputs for report and execute, or the acknowledgement changes.
Do not invent observations or replace unknown values with zero.

Only after explicitly accepting that exact loss report:

```powershell
vlite-server failover execute --target-file C:\private\authority-target.json `
  --ack-report-id <report.report_id> --expected-epoch <report.epoch> `
  --expected-revision <report.revision> --old-replicas 0
```

The acknowledgement is the exact `report_id`, epoch and revision, not a
general approval string. `--old-replicas 0` is an operator assertion; the
command does not inspect Azure. It refuses nonzero replicas with
`failover_fence_unconfirmed`, mismatches with `failover_ack_mismatch`, and
changed authority with `failover_report_stale` or `failover_conflict`.
For `failover_indeterminate`, inspect authority before any retry; never
blindly repeat a potentially successful transfer.

**Verify:** successful JSON contains `result.record` and `result.etag`.
The record has an incremented epoch, a new revision, phase `recovering`,
no writer ID, and the previous archived head as its base. Execute itself does
**not** restore, claim, start, route to, or enable serving on a new writer.
Start the replacement with the same authority target. Before opening its
database it pins and downloads the base, verifies the HA manifest, scope,
lineage and immutable index/chain, and replays exactly to `base.sequence`.
It syncs the staged file, moves an existing database aside using a no-replace
rename to `<database>.superseded-epoch<base.epoch>`, installs the restored
file and syncs its parent directory. The operation pin is released and staging
removed on success or failure.
An existing superseded path refuses startup; it is never overwritten.
Disposable ANN caches are removed so no stale cache is loaded.
Only then does the writer claim the observed epoch, with no CAS retry.
Commits after the verified base are not restored: they were never archived.
The superseded file preserves them for forensics only, is never served, and
operators may delete it after review. The next backup publishes a fresh HA
baseline at the new epoch. In a separately qualified recovery path,
verify restored data, sole ownership, authority, readiness,
tenant routing and a read/write check before admitting traffic;
`GET /v1/admin/serve-status` reports the claimed epoch only with authority
enabled (`null` otherwise) and is unavailable on a gateway-trusted backend,
where `authority show` reports the authority state instead; neither alone is
proof of recovery correctness.

**Back out:** before execute, cancel without changing authority, and resume
the old writer only after proving no handoff occurred and sole ownership
is intact. After execute or an uncertain result, leave old revisions
stopped. Do not decrement authority, overwrite it, or reactivate the old
writer as rollback.

## Add or replace a reader

**When:** adding isolated read capacity or replacing a stale/failed follower.
**Prerequisites:** a Published baseline and verified archive, an unencrypted
database, reader-scoped Blob permissions including operation pins, private
per-replica storage, and room for two follower generations plus staging.
Never mount the writer's database share on a reader.

Create `C:\private\reader-apply.json` with these fields (numbers below are
illustrative local inputs, not recommended defaults):

```json
{
  "target_file": "C:\\private\\archive-target.json",
  "follower_dir": "C:\\private\\follower",
  "capacity_bytes": 1073741824,
  "staleness_bound_ms": 60000,
  "poll_interval_ms": 1000
}
```

`target_file`, `follower_dir` and all three numeric fields are required.
`shared_key_file` is optional for the loopback emulator only. Unknown fields
and configuration over 64 KiB are refused. `follower_dir` must be absolute
without dot components. `capacity_bytes` is 1..1073741824 (1 GiB);
`staleness_bound_ms` is 1..3600000; `poll_interval_ms` is positive and
strictly below the staleness bound. These are implementation caps, not
ACA-qualified capacities.

The positional served path must be exactly
`<follower_dir>\reader-a\database.vlite`; the second private slot is
`<follower_dir>\reader-b`. Enrollment replaces the two slot directories:
use a dedicated follower directory, never a recovery copy to preserve.
With `VLITE_AUTH_TOKEN_FILE` set to the mounted private token file:

```powershell
vlite-server C:\private\follower\reader-a\database.vlite `
  --serve-mode reader --reader-apply-file C:\private\reader-apply.json
```

For a gateway-trusted reader, also supply
`--gateway-trust-file C:\private\reader-trust.json`, with the reader's
audience/control configuration. Do not set `VLITE_ADMIN_DIR`, create flags,
durability flags, group commit, ANN snapshots or gRPC on a reader.
Enrollment selects the newest Published set, pins the whole set, follows
verified history and swaps read-only generations.
When a writer authority exists in the target's coordination container, the
reader instead follows the set named by the authority head (or its frozen
base during a prepared transfer) and applies only checkpoints committed by
that head. A non-HA or mismatched manifest is refused with
`reader_apply_reseed_required`; an authority naming no set is
`reader_apply_no_set`. Without an authority, enrollment is unchanged.
An enrollment failure exits the process before it serves HTTP.

**Verify:** require `/ready` success and an authorized read of expected data.
Inspect administrator-only `/v1/admin/serve-status` for `mode: reader`,
`read_only`, lineage, binding generation and `reader` progress:
`applied_sequence`, `age_ms`, `bound_ms`, `stale`, `failed`.
A gateway-trusted reader grants only scoped tenant roles, so this route is
unavailable there. Use `/ready`, authorized reads, the freshness headers
below and private logs, and record the missing admin evidence as a
qualification gap. Do not expose the backend, add admin secrets or disable
trust to obtain it.
Authenticated reader responses carry `x-vlite-lineage`,
`x-vlite-binding-generation`, `x-vlite-applied-sequence` and
`x-vlite-applied-age-ms`. An HA reader also sends `x-vlite-epoch`, the epoch
of the archived head it follows, once known; the gateway falls back to the
writer when that stamp is missing or older than the tenant's epoch. Age is
since the last confirmation of the archived head, **not** writer-commit lag.
Sequence is withheld when memory bindings
block metrics; lineage/age can be absent before available. Anonymous probes
and authentication failures get none of these headers. They provide no
position token or read-your-writes guarantee.

No freshness confirmation or age beyond the bound causes 503 `reader_stale`
for readiness and authorized reads. A permanently failed follower fails
readiness with 503 `reader_apply_failed`; inspect private logs for the
reason. A newer baseline or broken continuity requires restart/re-enrollment,
not automatic reseeding.
For an HA reader that keeps running, a frozen base is not a freshness
confirmation, so age keeps growing during a prepared transfer and the reader
turns `reader_stale` at its bound. Enrollment always confirms freshness, so a
reader that restarts or re-enrolls during the transfer appears fresh again on
the frozen base. Keep replacement and re-enrolled readers out of routing
until the transfer completes. A transient authority read failure is retried without advancing; a
removed authority fails the follower with `writer_authority_missing`, and a
replaced or regressed head fails it until re-enrollment.

**Back out:** remove the reader from routing, stop it, and keep reads on the
writer (`read_routing: writer_only` in the directory). Keep ACA routing
writer-only until reader qualification passes. Never route writes to a
reader. Clean shutdown releases its operation pin; a crashed replica can
leave `reader-apply:<replica>` pins (maximum 16 per set). Remove abandoned
pins only after proving that replica is stopped, using the existing
[forced-unpin procedure](DOCKER-OPERATIONS.md#run-check-and-restore).
That procedure is an admin route, so it is unavailable on gateway-trusted
backends; there is no supported trusted-mode removal yet (a qualification
gap). Re-enrolling the same replica name replaces its pin; do not expose a
backend, add admin secrets or disable trust to remove one.
Restart with its dedicated private directory to enroll again; verify before
reintroducing reads.

## Recover control-state availability or publish a refresh

**When:** gateways/trusted backends fail readiness after a control-store
outage, or credentials/tenant routes need publication.
**Prerequisites:** authoritative admin directory, correct control target and
object, permissions, and a reviewed tenant directory file. Do not confuse
control policy generation with writer authority epoch.

Exact usage:

```text
vlite-server control-state publish|update --target-file <json> --object <name> [--shared-key-file <path>] --admin-dir <dir> [--directory-file <json>] [update only, at most one of: --revoke-credential <id> | --suspend-tenant <id> | --resume-tenant <id> | --reimport-if-generation <n>]
```

For first initialization only:

```powershell
vlite-server control-state publish --target-file C:\private\control-target.json `
  --object control.json --admin-dir C:\private\admin `
  --directory-file C:\private\directory.json
```

`publish` is create-only and refuses an existing object. For reviewed route
replacement or importing newly issued credentials/tenants:

```powershell
vlite-server control-state update --target-file C:\private\control-target.json `
  --object control.json --admin-dir C:\private\admin `
  --directory-file C:\private\directory.json
vlite-server control-state update --target-file C:\private\control-target.json `
  --object control.json --admin-dir C:\private\admin `
  --reimport-if-generation <current-generation>
```

A bare update is refused. Reimport requires the exact published generation,
else `control_state_generation_mismatch`; it preserves existing security
restrictions. Revocation/suspension/resumption options change the local
admin directory first, then CAS-update only that ID remotely. If publication
fails, the local change remains; retry that same mutation after repair,
not a blind reimport. `admin_dir_busy` means another command holds the
directory lock.

**Verify/recover:** restore store connectivity, identity permissions and the
valid object first; an outage alone needs no publication. Startup requires a
successful control read. Running gateway/trusted backend copies older than
`s_ctl_ms` fail closed with 503 `control_state_stale`; `/ready` also fails.
They poll and recover after confirming fresh state. Check `/ready` and its
`generation` on every gateway/backend, then verify both an allowed request
and a revoked/suspended denial. Returned mutation JSON reports `generation`
and `object`. A regressed generation is refused (`control_state_regressed`),
not accepted as recovery.

**Back out:** keep unavailable replicas out of traffic; never disable trust,
raise staleness to hide an outage, or restore an older control object.
Correct routes with another reviewed CAS update. Do not undo revocations
or suspensions merely to restore service.

## Rotate gateway signing keys with overlap

**When:** planned signing-key replacement.
**Prerequisites:** pinned current material/revisions, fresh immutable secret
names, space for both generations and a reviewed rollout/drain budget.
Exact key-generation usage:

```text
vlite-server gateway-keygen --kid <kid> --generation <n> --signing-key-out <new-file> --public-bundle-out <new-file>
```

Generate privately; existing output files are refused:

```powershell
vlite-server gateway-keygen --kid <new-kid> --generation <next-generation> `
  --signing-key-out C:\private\new-signing-key `
  --public-bundle-out C:\private\new-public-bundle.json
```

Follow the required
[manual staged overlap procedure](../deploy/azure-container-apps/ha/README.md#manual-staged-overlap-rotation-not-automated):
roll backends accepting old+new verification keys first, then gateways
signing with the new key. Keys/bundles load at process start; changing an
app secret alone is not a rollout. The gateway command is exactly
`vlite-server gateway --config <json>`; use the approved config, for example
`vlite-server gateway --config C:\private\gateway.json`.

**Verify:** require exact ready revisions and authorized requests at every
stage. Confirm all old signing replicas are stopped; wait the maximum old
envelope lifetime plus skew and in-flight/drain allowance before removing
old verification keys. Remove retired secret references only after all
referencing revisions/replicas are gone.

**Back out:** stop at the failed stage and retain overlap. A non-compromised
old signer may be restored only while all backends still accept it; never
drop an old verifier mid-rollout. Emergency compromise revocation can
deliberately interrupt service. Rotation is not automated, and the writer's
blocked handoff/qualification is not bypassed by key rotation.

## Restore an archive checkpoint offline

**When:** recovering an isolated copy to a verified durable checkpoint.
**Prerequisites:** a Published set with archive, scoped target access, private
password file, stopped destination server, and an existing private output
parent with sufficient free space. Output must not exist.

```powershell
vlite-server cloud-sets --target-file C:\private\archive-target.json
vlite-server cloud-restore --target-file C:\private\archive-target.json `
  --set <set-id> --archive-checkpoint latest --mode rekey `
  --output C:\private\restored --admin-username recovery-admin `
  --admin-password-file C:\private\admin-password
```

Exact usage excerpt: `[--archive-checkpoint latest|<sequence>]`.
Replace `latest` with an exact checkpoint sequence when required. Take it
from a retained checkpoint report or the archive status; `cloud-sets` lists
sets, not checkpoint sequences. A refused value's message lists the
available sequences.
Times, intermediate sequences and rounding are refused
(`pitr_target_unsupported`). No archive gives `pitr_archive_missing`.
For an HA set, `latest` and the listed sequences are only the checkpoints
committed by the authority head (or base) for that set.
Only rekey is supported: new lineage, imported accounts disabled, no
credentials/sessions restored; seed mode is refused. This is not a
same-lineage writer promotion.

**Verify:** inspect the JSON report and `archive` checkpoint details, then
validate the isolated data and fresh admin sign-in before selecting or
routing anything. Nothing starts or selects automatically. Reissue
credentials and review restored memory bindings. Exit 3
`restore_cleanup_failed` means complete output but an unreleased pin,
not an absent restore. Follow the existing
[checkpoint restore and limits](DOCKER-OPERATIONS.md#continuous-log-archive-and-checkpoint-restore-opt-in)
and [restore security procedure](DOCKER-OPERATIONS.md#run-check-and-restore).

**Back out:** leave production routing/files untouched; keep or discard only
the isolated output after inspection. Never overwrite the old database to
test recovery or reuse old credentials on the new lineage.

## What is not provided

- Automatic failover, or an ACA-qualified, deployment-orchestrated completed
  fenced handoff; `failover execute` itself only prepares the transfer.
- Multi-writer operation or additional write capacity.
- Cross-region failover or geo-failing the authority.
- Power-loss-qualified Azure Files or production availability/durability SLOs.
- ACA gateway load and staleness measurements, or a completed ACA run of the
  Blob fencing measurement.
- Query routing of writes to readers, position tokens or read-your-writes overrides.
