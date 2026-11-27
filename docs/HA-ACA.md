# Opt-in HA operations on Azure Container Apps

These are experimental operator procedures, not production qualification.
**HA is opt-in.** With none of the HA flags or files, VLite runs exactly as
the single-writer deployment does today: no new headers, fields, reads,
errors or startup requirements.

Gateway and reader scale-out add read capacity and isolation, **not writer
HA**. There is one writer. The writer serving fence is **explicitly opt-in**,
and there is no qualified automatic failover. Failover is an
operator-run, reported and acknowledged procedure; the command prepares
authority transfer, not a running replacement. An unqualified
[automatic failover supervisor](#automatic-failover-unqualified-off-by-default)
exists for isolated drills only; it is off by default.

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
- **Gateway load (MEAS-004):** Isolated ACA runs collected proposal evidence
  only. The latest full-profile run started 2,017,242 of 2,076,000 scheduled
  requests but did not reach the offered 1,600 requests per second through the
  gateway, and 27 HTTP 503 responses have no identified cause. This is not
  capacity, overhead or SLO evidence; see the
  [measurement record](../deploy/azure-container-apps/ha/measure.md).
- **Control-state changes (MEAS-005):** Not measured on ACA.
- **Writer fencing (MEAS-006):** In-memory and local Blob emulator runs
  exercised the writer checks. Both ACA attempts reached log collection
  without a usable result; the cause is unknown. Neither fencing latency on ACA nor a bound
  on how long a stale writer can acknowledge writes has been established.
- **Reader sizing (MEAS-007):** Temporary ACA jobs measured resources during
  reader apply, index rebuild and compaction on test data. These were not
  serving workloads and do not establish a supported tenant size.

These are not availability, recovery-time or data-loss guarantees. There is
no qualified automatic failover. The dashboard shows the server's HA role and
replication status and the off-by-default switch for the
[unqualified supervisor](#automatic-failover-unqualified-off-by-default);
it does not perform failover. Use the operator procedures below;
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
Even a restart requires an explicit, acknowledged operator failover, unless
an operator turned on and runs the unqualified
[automatic failover supervisor](#automatic-failover-unqualified-off-by-default).

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
vlite-server failover report|execute --target-file <json> [--shared-key-file <path>] [--archive-status-file <json>] [--observed-sequence <n> --observed-unix-ms <ms>] [execute only, all required: --ack-report-id <id> --expected-epoch <n> --expected-revision <rev> --old-replicas <n>]; unqualified automatic failover (GATE-005 open, off by default): vlite-server failover auto --config <json> | failover policy show|enable|disable --target-file <json>
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

## Automatic failover (unqualified, off by default)

> **Automatic failover is unqualified: GATE-005 has not passed.** When it is
> on, a supervisor that loses contact with the writer fences it and transfers
> authority without a person reviewing the loss report, so recent commits
> can be lost. Use it only in an approved isolated drill, never for
> production.

**When:** exercising unattended detection and transfer in an isolated
environment. **Prerequisites:** everything required for the manual
[writer failover](#planned-or-unplanned-writer-failover), plus a fence command
that reliably stops **every** old writer replica and proves it, and exactly
one operator-run supervisor per authority target. Nothing changes until the
switch is on **and** a supervisor is running.

**Switch it on or off.** One policy object beside the authority record
(`<prefix>auto-failover.json`) is the switch for every supervisor of that
target. A super administrator uses the dashboard under
**Administration → Backup & recovery → Automatic failover**, the REST route
`GET`/`PUT /v1/admin/auto-failover` on the writer (see the
[API reference](API.md#automatic-failover-unqualified-opt-in-off-by-default)), or:

```text
vlite-server failover policy show|enable|disable --target-file <json> [--shared-key-file <path>] [enable only: --expected-revision <rev> (when a policy exists) --acknowledge-unqualified true]
```

Turning it on needs the unqualified acknowledgement and the revision last
read (32 lowercase hexadecimal characters, required once a policy exists),
so two operators cannot silently overwrite each other. Turning it off needs
neither and always writes a fresh disabled revision, even if the policy is
already off or missing; this invalidates an enable based on an older
observation. It is the **kill switch**. The command-line
`failover policy disable` is the primary kill switch because it needs neither
the dashboard nor a running writer. A missing or unreadable policy counts as
off; if the store cannot be reached, supervisors hold with
`policy_unavailable` and fence nothing. Supervisors read the switch before
every probe, immediately before fencing and again after fencing. They check
it once more after building the loss report, immediately before authority
transfer. Disabling before that final check aborts promotion; a transfer
already past that check may still complete. Disabling cannot recall a fence
command that is already running.

**Run a supervisor** with a private config file of at most 64 KiB; unknown
fields are refused:

```json
{
  "target_file": "C:\\private\\authority-target.json",
  "writer_ready_url": "https://<writer-host>/ready",
  "probe_token_file": "C:\\private\\probe-token.txt",
  "probe_interval_ms": 5000,
  "probe_timeout_ms": 2000,
  "failure_threshold": 6,
  "fence_command": ["pwsh", "-NoProfile", "-File", "C:\\private\\fence-writer.ps1"],
  "promote_command": ["pwsh", "-NoProfile", "-File", "C:\\private\\start-replacement.ps1"],
  "command_timeout_ms": 300000
}
```

```powershell
vlite-server failover auto --config C:\private\auto-failover.json
```

`target_file` and `shared_key_file` (optional) are the same as for
`failover report`. `writer_ready_url` is http or https; use the writer's
`/ready`, not `/health`, which always succeeds. `/ready` needs a credential
unless the writer trusts a proxy: `probe_token_file` (optional) holds one
bearer token of any role, at most 4096 printable ASCII characters. Only
HTTP 200 with JSON `status: "ready"`, `mode: "writer"` and the expected
authority `epoch` counts as healthy. HTTP 401 or 403 holds the supervisor
with `probe_unauthorized`, a redirect with `probe_redirected`, throttling
(429) with `probe_throttled`, and a mismatched or malformed identity also
holds instead of counting as a failure. Redirects are not followed.
`probe_interval_ms` is
500 to 60000,
`probe_timeout_ms` is 100 or more and below the interval, and
`failure_threshold` is 2 to 100 consecutive failures. These are examples,
not qualified timing defaults. Each command is an argument list of 1 to 64
arguments run **without a shell**, with no standard input, a time limit
(`command_timeout_ms`, 1000 to 3600000, default 300000) and at most 64 KiB
of standard output; its standard error passes through. A command must not
leave a background process holding its standard output open. Each command
receives the writer it acts on in `VLITE_FAILOVER_EPOCH`,
`VLITE_FAILOVER_REVISION` and `VLITE_FAILOVER_WRITER_ID`; `promote_command`
also receives `VLITE_FAILOVER_NEW_EPOCH`.

Run **exactly one** supervisor per authority target; nothing stops a second
one. Turn the switch off before any planned maintenance, restart or manual
failover of the writer, or the supervisor may fence it. The switch is shared
by the target; starting multiple supervisors does not create independent
switches.

**What it does.** The supervisor counts consecutive failed probes for the
same authority epoch and revision; a healthy probe or any authority change
restarts the count. It acts only on a serving writer that it has itself seen
ready in the same epoch, so a probe URL that never reached the writer cannot
fence it; otherwise it holds with `writer_never_seen_healthy` or
`no_serving_writer`. If the store cannot return the switch or the authority
record, it holds with `policy_unavailable` or `authority_unavailable`. At
the threshold it:

1. Reads the switch and the authority record again; if the switch is off it
   stops counting, and if authority changed it holds. Either way it fences
   nothing.
2. Runs `fence_command`, which must stop all old writer replicas, re-enumerate
   them and print exactly `{"old_replicas":0}`. This is the same operator
   assertion as `--old-replicas 0`: the supervisor cannot inspect Azure, so
   the fence command's proof is the safety boundary.
3. Checks the switch again.
4. Takes a loss report as `failover report` does and refuses if the epoch
   or revision changed.
5. Re-reads the switch immediately before transfer. If it is off, promotion
   aborts. This is the disable cutoff; a transfer already past this check may
   complete.
6. Runs the same transfer as `failover execute`, acknowledging that report.
7. Runs `promote_command`, if any, to start the replacement writer.

It makes **one** attempt and then exits. It never restores, verifies or
routes traffic itself; the replacement starts exactly as after a manual
`failover execute`, and the same verification applies before admitting
traffic. Supervisors log `auto_failover_started` and each changed
`auto_failover_tick` to standard error.

**Verify:** a promotion prints JSON with `state` `promoted`, `old_epoch`,
`new_epoch`, `report_id`, the full `report`, its `text`, and `standby`
(`started` or `not_configured`), and exits 0. The same JSON is logged as
`auto_failover_tick`. Retain that output and log privately and review the
disclosed loss exactly as for a manual failover. Then verify the replacement
as in [writer failover](#planned-or-unplanned-writer-failover).

If authority moved but `promote_command` failed, the supervisor exits
nonzero with `auto_failover_standby_failed`, naming both epochs and the
report; the logged tick has `standby` `start_failed`. The replacement's
state is unconfirmed: the command may have partially succeeded or timed out
after starting it. Inspect the authority and actual writer replicas before
deciding whether to start a writer manually as after `failover execute`.

Any other outcome exits nonzero with `auto_failover_aborted: <reason>`:
`fence_failed` or `fence_unconfirmed` (the fence command failed, or did not
report zero), `disabled_during_promotion`,
`policy_unavailable_during_promotion`, `report_failed`, `authority_changed`
(another operator or supervisor moved authority first), or
`transfer_failed`. After any abort the old writer may already be stopped.
After `transfer_failed` the transfer may have happened: inspect authority
before any retry; never blindly rerun the supervisor.

**Back out:** turn the switch off and stop the supervisor. Before a
transfer, resume the old writer only as the manual back-out allows. After a
promotion or an uncertain result, follow the manual back-out: leave old
revisions stopped and never decrement or overwrite authority.

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

`isolated_pins` is optional (default `false`). When `true` the reader pins
with its own object `<prefix>reader-pins/<set>/<pin>.json` in the
coordination container instead of rewriting the set record, so it writes no
set record, authority or other control object and no data object
(design-tenant-provisioning ITEM-008; tested offline only). Enable it only
after every writer and janitor on the prefix runs a version that honours
reader pin objects: an older janitor ignores them and could collect a
non-head set the reader still follows. Azure RBAC/ABAC scoping of a grant to
that path is **not qualified**; this setting narrows what the reader needs,
not what a grant permits. Enrollment in this mode replaces only its own
reader pin objects; a record pin from earlier legacy runs stays until
removed. The forced-unpin route does not see reader pin objects.

`New-VLiteHaReaderStorageGrants` (`deploy/azure-container-apps/ha/templates.ps1`)
is an offline, pure builder for the proposed per-tenant reader grants on the
existing storage account. It returns one custom pin-writer role and two
condition-version 2.0 assignments for the reader identity. The custom role
has blob data actions only (read/write/add/delete) and no control-plane
actions, so no container delete/write. Its name and GUID are derived
deterministically from the resource group. Each assignment also has a
deterministic GUID `name`, derived from its lowercased scope, principal ID and
role definition ID, so rebuilding the template yields the same role definition
and assignment names. On the coordination container it allows writes and deletes only
under `<prefix>reader-pins/`. The data container uses the built-in Storage
Blob Data Reader role with reads only. Both assignments allow reads only under
`<prefix>`, and lists only when the request prefix starts with `<prefix>`.
User delegation keys are a storage-account-level action, so these
container-scoped assignments cannot mint them. The builder requires
managed-identity targets, refuses `shared_key_file`, and refuses a sibling
tenant prefix that nests with (or equals) this one, or any input with a
trailing newline. The sibling list is asserted by the operator
(`sibling_inventory:"operator_asserted_complete"`), not discovered. The
conditions bind the principal object ID; `client_id` is accepted but unused.
Readers of one tenant are cooperative: any of them can create or delete that
tenant's pin objects, so a stale-pin TTL or lease and a per-reader pin segment
remain follow-ups. It invokes nothing and its result reports
`offline_template_effective_access_unqualified`. Azure enforcement of these
conditions, whether every reader list carries the prefix, role-assignment
propagation and mount behaviour are all unqualified, and present reader fleets
still refuse `pin_scope_unproven`.

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

### Provisioned reader fleets (offline contract, not available)

Dedicated-tenant reader lifecycle (`set_readers`) exists only as offline
building blocks tested against fake ARM and test-only proofs. No REST/CLI or
control-worker path runs it, and every present-fleet request still returns 409
`pin_scope_unproven`. Readers stay out of routing: tenant routing must be
`writer_only` (otherwise `reader_routing_unqualified`) and no reader grant is
issued. Use the manual procedure above. The contract below records what a
future qualified path must keep.

- **Desired vs observed:** the desired reader count, profile and private
  endpoint are fixed per operation in its durable plan. Observed state is the
  ACA app readback plus a fleet observation of each replica. A reader becomes
  eligible only when the writer authority is valid and serving with a head
  (otherwise `reader_base_transition`), the tenant's reader app is owned and
  runs the desired configuration in one active revision
  (`reader_revision_unproven`), and every replica runs in that app and
  revision, follows the same set, lineage, binding and epoch, is not failed,
  and is fresh within its bound (`replica_ineligible`). Eligibility is proven
  again from a current observation on every attempt before an endpoint is
  published, so a retry never publishes on an earlier attempt's proof. Only
  a publication that already committed is recovered from its receipt without
  a new observation. Missing observations return
  `fleet_observation_unavailable` and duplicate replica names return
  `replica_identity_conflict`. More replicas than the maximum gives
  `reader_fleet_not_reduced`, fewer than the minimum `reader_fleet_below_min`.
  The maximum is a cost limit, not an availability or latency objective.
- **Anchoring:** every reader change names the tenant's latest succeeded
  receipt and current route (`readers_unanchored` otherwise). The
  reservation itself is taken only if the lifecycle slot is still released by
  the same operation and the route is unchanged, checked in the same
  compare-and-swap. A plan made before another change sealed or was
  cancelled is refused without taking the slot, even if the route is the
  same. Retirement is bound the same way (`retirement_receipt_mismatch`).
  A same-operation retry that already holds the slot continues only if the
  slot was reserved against its plan's route.
  REST/CLI submission of a reader change or retirement records the operation
  (phase `accepted`, `observed.reserved` false) but reserves nothing: only the
  plan-anchored actuator reserves, so a submission never blocks or poisons
  later anchoring. Only `create` is reserved on submission. Cancelling an
  accepted, unreserved reader change or retirement withdraws it in the same
  control-state compare-and-swap that would reserve it, so exactly one of the
  two wins. The withdrawal is a permanent marker in the control state, not a
  journal flag. The operation is sealed `cancelled` and can never reserve
  (`operation_terminal`), even on retry or after a restart. The slot, route
  and latest receipt stay as they were, so the next change anchors as before.
  If the reservation wins, the cancel goes through that reservation instead
  (refused with `readers_in_progress`, `retirement_in_progress` or
  `retirement_required` once the change has progressed) and the operation is
  not withdrawn. Slots reserved without a
  plan by builds from EPIC-002 through e36403e1 are refused when their route
  differs from the plan. One reserved at the same route after a newer receipt
  is indistinguishable (a reserved slot records no predecessor) and can be
  accepted by the same-operation retry path. Once cancelled, such a slot
  leaves no plan to anchor on, so later changes stay refused (fail closed).
  The missing history is not repaired. The first
  endpoint is published by compare-and-swap. A later operation must keep the
  same endpoint (`reader_endpoint_changed`). Reader apps are private copies
  owned by the plan. A writer app or a different reader gives
  `reader_app_mismatch`; writer and reader app IDs are compared ASCII
  case-insensitively (as ARM compares them), so a reader ID differing from a
  writer ID only in letter case is refused too, including in a persisted plan
  or anchor, before any endpoint change or ARM call. A deleted planned reader
  gives `reader_app_missing`.
  The writer's identity and intent are never changed. **Not enforced:** a
  reader profile must never mount the writer's storage. The check above only
  compares app names, and the profile schema that would expose mounts
  (OD-006) does not exist, so this is blocked, not implemented.
- **Removal (`absent`):** the reader endpoint is unpublished first, before any
  reader app is stopped. Once unpublished, the removal cannot be cancelled
  (`readers_in_progress`). It completes only after every reader app is proven
  stopped (`readers_incomplete` otherwise).
- **Scale-in (smaller `present` fleet):** the reader app is updated in place
  while its endpoint stays published. Nothing is withdrawn first. The change
  seals only once the reduced fleet is observed eligible
  (`reader_fleet_not_reduced` until then). Reads stay on the writer only
  because reader routing is disabled (`writer_only`), not because of the
  reduction. Before reader routing can be qualified (ITEM-021), a present
  reduction must withdraw the endpoint first and a cancelled update must be
  rolled back. Neither exists. An uncertain stop or update stays unknown and
  is never resent or reported as success.
- **Abandoned pins:** control may release only `reader-apply:<replica>`
  operation pins created before a proven stop of that replica
  (`replica_stop_unproven` without proof). Each release is journaled first and
  repeated runs are idempotent. This also frees a full set (16 pins) left by
  crashed replicas. Unknown termination stays refused. Never release a pin
  of a replica that may be running. The stop proof must name this tenant and
  one of its planned reader apps. The pins themselves carry only the replica
  name: no tenant, app or lineage. Recovery therefore scans every set, and
  replica names can collide (the follower falls back to the host name or
  `pid-<pid>`). An opt-in isolated-pin layout and offline tenant-scoped grant
  template exist (ITEM-008), but they do not add tenant identity to pin
  records or prove effective production scope. A production observation and
  stop-proof source does not exist.
- **Cleanup debt:** cancelling a reader add deletes only the reader it
  created. A cancelled reader update is not rolled back: the app keeps the
  new configuration while the route is unchanged. A stopped
  reader app is retained and reused by the next scale-out. An uncertain pin
  release is resolved only by rerunning recovery. Retirement must name the
  latest receipt and also stops owned readers.
- **Missing production proof (all refuse):** reader pin scope (ITEM-008),
  qualified ARM preflight and stop/update evidence, a replica observation source,
  replica clock proof, reader qualification, profile/quota/budget (including
  the writer-storage mount check, OD-006), reader credentials and the
  control-worker/REST composition.
- **Replica replacement and route drift:** there is no targeted replica
  replacement. The local fixture dispatches one guarded in-place update,
  without resending it. One failed replica keeps the change unsealed until
  an all-healthy fleet is observed on the same active revision used for the
  failed observations. This does not prove that an unchanged configuration
  rolls a new ACA revision or triggers replica replacement. Replica-side lag
  (stale age, older binding or epoch, replaced revision) is
  `replica_ineligible`. Control-side lag, meaning a route changed outside the
  lifecycle (for example a binding-generation advance through
  `control-state update --directory-file`), matches no receipt. Later reader
  changes then refuse `readers_unanchored` and retirement refuses
  `retirement_receipt_mismatch`, with no slot or effect. Nothing repairs this
  implicitly. The fixture restores the receipt's route to isolate the anchor
  check; that is not an operational rollback procedure. Republish that route
  only after verifying it still names the authoritative current writer,
  epoch and binding. Never undo an intentional rebind or writer/epoch move
  merely to match a receipt; otherwise leave the state unchanged and
  escalate. Reader-pin/Azure RBAC policy-propagation lag is not modelled by
  these fixtures and remains open under ITEM-008/ITEM-019.

## Managed tenant deployments (local intent only, not ACA-qualified)

The `tenant-deployment` admin surface records durable, idempotent lifecycle
**intents** in the control-state store. It does not provision, start, stop,
deploy or route any Azure resource. The ARM adapter is built only behind
every worker gate, and there is no qualified ACA behavior. The [control worker](#control-worker-and-aca-job-offline-only)
(`work`) refuses at its gates before any claim or cloud call, so it runs no
accepted operation in production. For managed tenants today, use the manual
procedures in this guide.

| Request | What runs locally today | Qualified on ACA |
|---------|-------------------------|------------------|
| `create` | Records the intent, creates **unrouted** tenant metadata from the persisted ID and reserves the tenant (phase `reserved`). Cloud execution refuses (OD-002). | No |
| `status` | Read-only view of the records (below). | No |
| `work` | Runs the gates for one accepted `create`. Production refuses before any claim or cloud call, at the latest with `qualification_missing` ([control worker](#control-worker-and-aca-job-offline-only)). Only offline fixtures claim and run an attempt. | No |
| `resume` | Records the action as an idempotent no-op; it never runs a cloud step. A `present` reader change answers 409 `pin_scope_unproven`. | No |
| `cancel` | Seals and releases a reserved operation that has not activated (`retirement_required` once activated). Withdraws an unreserved reader change or retirement so it can never reserve; an unreserved `create` or `migrate` answers `transition_unavailable`. | No |
| `readers`, `retire` | Recorded as intent only (`accepted`, `observed.reserved: false`). The actuators exist only in offline fixtures. | No |
| `migrate`, `rollback`, `release` | Excluded (see below). | No |
| `issue-credentials`, `acknowledge-delivery`, `replace-credentials` | REST only for minting (the CLI refuses `issue-credentials` and `replace-credentials`). Deliver a reserved, seeded create's candidate credential once in the admin response, acknowledge it or replace a lost one ([API](API.md)). Production has no seed recorder yet, so issue answers `seed_unrecorded`; delivery never routes the tenant. | No |

**Before you start.** REST is off by default. Without the flag, authenticated,
well-formed requests answer 409 `tenant_deployments_not_configured`;
authentication and body validation can reject a request before this check. Start with
`vlite-server <db> --tenant-deployments-file <json>`, where the JSON is
`{"target_file", "object", "shared_key_file"?}` (the same control-state
source as `control-state`). This also needs `VLITE_ADMIN_DIR` and an admin
token, and refuses `--insecure-local` and `--durability none`. While it runs,
the server holds the admin directory's lock, so the CLI answers
`admin_dir_busy`. Use either the server or the CLI, and retry through the
same surface and principal; they do not share an idempotency namespace:

```sh
vlite-server tenant-deployment create --request-file req.json \
  --target-file target.json --object gateway/control-state.json --admin-dir /data/admin
vlite-server tenant-deployment <readers|retire> --tenant <id> --request-file req.json \
  --target-file target.json --object gateway/control-state.json --admin-dir /data/admin
vlite-server tenant-deployment status --operation <id> --target-file ... --object ... --admin-dir ...
vlite-server tenant-deployment work [--operation <id>] --target-file ... --object ... --admin-dir ...
vlite-server tenant-deployment <resume|cancel> --operation <id> --request-file action.json \
  --target-file ... --object ... --admin-dir ...
```

`--tenant` is required for `readers`/`retire` and refused for `create`.

Operation records live under `<prefix>tenant-operations/` beside the
control-state object. Routes, bodies and errors are listed in the
[API reference](API.md#managed-tenant-deployments-intent-only-opt-in-off-by-default).

**Dashboard.** A super administrator signed in to the server directly can
list the operations and their phases under **Administration → Users &
access → Tenant deployments**, 20 at a time. The same panel can also
submit `create`, `readers` and `retire` intents, check an operation's status
and `cancel` it. Like the routes above, it only records the request. Cloud
execution stays switched off (OD-002), so nothing is deployed, routed,
started or stopped in Azure, and reader fleets stay blocked until ITEM-008. Every
submission asks for confirmation first, and each field explains where its
value comes from:

- Profiles are chosen as files or pasted. The browser computes their
  SHA-256 digests and sends the exact text in `contents`. The request
  inspector shows only counts and profile digests for the submission body,
  not the documents, Azure resource names or secret-version references.
  If authentication ends, the dashboard clears the inspector.
- **Use this operation** in the list fills in the operation, the tenant and,
  when one exists, the retirement receipt.
- Cancel always reads the latest status first and sends the tokens it just
  read. A stale token is explained, and **Check status** reads it again.
- A cancel seals the operation before it releases the reservation. If the
  release fails, status shows `cancelled` with `observed.reserved` still
  true and the next step `cancel again to release the reservation`. An
  exact resend of that cancel (same account, same key) finishes the
  release. The server also accepts a new cancel from any administrator that
  quotes the operation's current tokens, but only while this operation still
  holds that reservation. Once it is released, or for an operation that
  succeeded or retired, a new cancel is refused (`operation_terminal`). The
  panel saves each cancel it sends in this browser tab's `sessionStorage`,
  filed under the API address the dashboard sends its requests to, then the
  account the server verified at sign-in: a user account by its ID, or the
  bootstrap admin token (everyone using that token counts as one account). A
  role alone never counts as an account. The record holds the operation ID,
  the request key and the values it quoted, never a password or token. It is
  plain text, though, so anyone using the tab can read it until it is
  deleted.
  - **Same account after a reload:** **Cancel** resends it exactly. A password
    session that has not ended resumes by itself; a bootstrap token user
    types the token again.
  - **Another account in the same tab** cannot resend it. The panel says this
    tab has no saved cancel for the account signed in now, and **Cancel**
    sends a new cancel instead (see below). The first account's record stays
    until that account signs out or its session ends.
  - **Another dashboard on the same site** cannot resend or delete it.
    `sessionStorage` is shared by every page of a site in a tab, and one site
    can serve dashboards for different servers. For example, the Docker image
    reaches its API at `/api`, and another dashboard could use a different
    path. So records are filed by the exact API address the requests go to:
    scheme, host, port and path. The address never includes credentials, a
    query or a fragment. If the dashboard cannot work out that address (or
    the address includes any of those), the cancel is not saved.
  - **Deleted** when its account signs out, and when its session is seen to
    end: a 401 (expired or revoked session, rejected token), a typed token
    replacing this browser's password session, or a page load that finds no
    password session (user accounts only). A typed token refused with a 401
    (even one without a JSON body) also deletes the bootstrap admin token's
    records for this API address, even if the dashboard had not yet learned
    who signed in. Closing the tab deletes it too. On every sign-in and
    sign-out, including one where the dashboard does not know who was signed
    in, the panel deletes and never uses records it cannot attribute to an
    account, such as ones saved by an older version of the page, or damaged
    ones.
  - **No verified account or no API address:** the cancel is not saved; it is
    kept only while the operation stays selected on the page.
  - If the saved cancel is gone (another tab or browser, a sign-out or an
    ended session), the panel says so and **Cancel** sends the new cancel
    described above instead: a new request key and the tokens from the
    status it reads just before sending. If another account sent the
    earlier cancel from this tab and its session has not ended, that account
    can instead sign in again and resend it exactly.
  - **Refused new cancel** (`operation_terminal`): either another
    administrator or tab finished the release in between, or the server is
    older than this recovery. The panel says to select **Check status**
    first; nothing more is needed if the reservation is now released. If it
    is still held, only an exact resend works on that server, so the account
    that sent it can sign in again in this tab, or else report the operation
    ID to the server operator. **Cancel** then stays off for that operation
    until the page reloads or someone signs in or out.
- Each **Create** adds a tenant record at once. The record counts toward the
  tenant limit and stays even if the request is later cancelled.

Network errors, 408, 429, 5xx, or any code not on the settled list keep the
key: selecting the same button again resends the unchanged request with the
same key, so it is recorded only once. A success answer that does not name
the requested operation or tenant is also treated as unknown. Settled codes
discard the key. Some of them (for example `tenant_id_conflict`) can follow a
recorded request, because a resend repeats the same refusal; their
explanation says the request may stay recorded and to check **Load
operations**. `invalid_input` also keeps the key, because the request may be
recorded before the server refuses to add its tenant record. Resending at
once fails the same way, so the explanation says to fix the cause first and
then select the same button without changing the form. For `tenant capacity
reached`, the server already holds 1,024 tenant records, deleted tenants
included: report it to the server operator. Editing the form starts a new
request. The panel
stays locked with open development access (no admin token), for reader and
tenant-scoped sign-ins, and through the HA gateway, which refuses admin
routes. `resume` is available only through the CLI or REST; migration and
the other actions stay excluded (below).

**Create.** Submit `{"idempotency_key", "request": {"kind": "create", ...},
"contents": [...]}`. `contents` holds the exact text of every profile it
names by SHA-256. Resubmitting the same key and content through the **same
surface and principal** returns the same operation (and reserves it if an
earlier attempt stopped before that). The CLI uses `cli:tenant-deployment`;
REST uses the authenticated admin principal. Switching surface or principal
creates a different operation and can allocate another unrouted tenant.
Within the same principal's namespace, the same key with different content
answers `operation_request_conflict`. The
tenant stays unrouted, and status keeps `next_step` at "cloud execution is
blocked (OD-002)". Profiles, quota and budget have no approved schema
(OD-006), and no credential view is delivered (OD-004).

**Status.** `GET /v1/admin/tenant-deployment-operations/{operation}` (or
`status`) shows `phase` (`accepted`, `reserved`, `cancelling`, `activated`,
`succeeded`, `cancelled`, `retired`), `next_step`, the generation tokens
(`operation_generation`, `lifecycle_generation`, `receipt_digest`),
`desired`, and `observed` (`reserved`, `routed`, `cancellation_requested`,
`journal_steps`, `uncertain_steps`, `terminal`). It never shows the
idempotency key, profile contents, secrets or tokens. `observed` reports the
operation records, not live Azure state.

**Resume and cancel.** Each action is
`{"idempotency_key", "expected_operation_generation",
"expected_lifecycle_generation", "expected_receipt"?}` with the tokens from
the latest status. Stale tokens answer 409 `operation_generation_stale` (read
the status again). The same key with a different request answers
`action_request_conflict`. A refused action writes no record. `resume` is not
cloud execution. Cancelling a reserved `create` seals the operation and
releases the reservation. If status shows `cancelling`, repeat the same
request. A cancelled create keeps its non-routable tenant metadata.
Cancelling an accepted, unreserved reader change or retirement **withdraws**
it atomically with its reservation and seals it `cancelled`. It can never
reserve afterwards, and the slot and route are left as they were. If its
reservation won first, the cancel follows the reserved reader and retirement
rules instead (see the anchoring contract above) and nothing is withdrawn.
The control-state object keeps at most 1,024 withdrawals across all tenants.
They are never pruned, because pruning would let a withdrawn operation
reserve; past the limit, cancel answers 409 `withdrawal_capacity` and writes
nothing. An unreserved `create` or `migrate` still answers
`transition_unavailable`.

**Readers and retirement.** Submissions only record intent. Present fleets
stay denied (`pin_scope_unproven`, ITEM-008) and reader routing stays
`writer_only`. The offline contract that a future qualified actuator must keep
is in [provisioned reader fleets](#provisioned-reader-fleets-offline-contract-not-available).
It covers removal (unpublish before stop), present scale-in (in place, with
the endpoint still published), anchoring and cleanup debt. The retirement
contract removes the route and stops apps. It never deletes data
(`data_disposition` accepts only `retain`).

**Migration and release: excluded.** Existing-tenant migration (EPIC-006) is
not implemented and is outside this runbook. Do not submit `migrate`: the
route only records an unreserved intent and nothing runs it. `rollback` and
`release` (which first needs `ack_irreversible: true`, otherwise 422
`irreversible_ack_required`) answer 409 `transition_unavailable`, as does
credential delivery for a migration. There is no data
barrier, cutover, migration credential delivery or source release. Move tenants only
with the reviewed manual procedures.

**Retained data.** Cancellation and retirement keep databases, archives,
writer authority, credentials and control records. In the offline contract,
cancellation deletes only apps that the operation itself created, and
retirement stops apps and removes the route. A resource that existed before
the first dispatch is refused, never adopted, even with the owner tag. Today
no app is created, so an admin cancel deletes nothing. Do not delete
operation records, edit lifecycle entries by hand or remove control state to
"unstick" an operation.

**Uncertain outcomes.** `uncertain_steps > 0` means a cloud step's outcome is
unknown. Today only offline fixtures can produce one, because no adapter
runs. The contract reconciles through the persisted ARM operation and
readback. It never resends an unknown action, never adopts a foreign
resource and never retries destructively to clear debt. A cancel cannot seal
while an action or guard is unresolved. Slots reserved without a plan by
local builds from EPIC-002 (1d8000b9) through e36403e1 lack provenance and
have no automated repair. A differing route refuses. A same-route slot with
a newer receipt may be accepted by a same-operation retry. After it is
cancelled, later changes refuse (fail closed). Leave such state in place and
escalate.

**Schema upgrade and rollback.** The control-state object gains a
`lifecycle` field only once an operation reserves, and a `withdrawn` field
only once an unreserved operation is withdrawn; until then its bytes are
unchanged for older readers. Older binaries that refuse unknown fields fail
closed on an object that carries either. They do not ignore it. Locally this
is shown with strict pre-lifecycle and pre-withdrawal field sets, not by
running an old binary. Rolling back therefore means rolling the binary
forward again or keeping the new version. Never strip `lifecycle`,
`withdrawn`, reservations, receipts or operation records to make an older
binary start: stripping `withdrawn` would let a cancelled operation reserve.

**Effective permissions are not proven.** Legacy reader pins rewrite the
writer's set record, so that path cannot use a pin-only tenant grant. The
opt-in isolated-pin layout and offline per-tenant grant template now exist
(ITEM-008), but fake-store and template tests prove neither Azure RBAC/ABAC
enforcement nor mount isolation. Keep isolated mode disabled until every
writer and janitor on the prefix is upgraded and the cloud grant is qualified.
Writer-storage mount exclusion is also not enforced (OD-006). Treat every
permission statement here as a design requirement, not a verified property.

**Local fault suites.** These run without Azure and are the only evidence for
this surface:

| Command | Shows | Does not show |
|---------|-------|---------------|
| `cargo test --features server --lib tenant_provisioning` | Durable intents and idempotency, reservation/route CAS, guards, fake-ARM create/update/delete/stop, unknown-outcome reconciliation, no adoption, retention on cancel/retire, reader anchoring/eligibility, schema compatibility, admin REST/CLI refusals and redaction | Real ARM/ACA behavior, Azure RBAC or mounts, clock/freshness, stop proofs, timing or storage measurements |
| `cargo test --features server --lib epic_005` | The reader/retirement subset, including stale plans, races, withdrawal-versus-reservation races, failed-to-healthy fleet observations and route drift | ACA replica replacement, reader-pin/RBAC policy-propagation lag (ITEM-008/019), reader routing or reader qualification (ITEM-021) |
| `cargo test --features server --lib tenant_provisioning::worker` | The control worker: one claim and one attempt per invocation, every gate refusing in order before the claim and the adapter, production never building the adapter without proofs, one test-proof attempt provisioning a writer through `ArmClient` on a loopback fake (`tenant_provisioning::arm`), a lost claim race and a foreign claim refusing `actuator_fence_unproven`, a refused dispatch reported `failed`, an unknown dispatch kept uncertain and never retried, cancelled operations, unknown IDs and store faults failing closed, no control-state write | Real Azure ARM, `ActuatorStopped` proofs, polling pace, a Job execution on ACA |
| `pwsh -NoProfile -File deploy/azure-container-apps/ha/test.ps1` | The worker Job template (TEST-014): manual trigger, parallelism 1, one completion, `replicaRetryLimit` 0, a dedicated identity that no serving role holds, only the control-target secret, the `work` invocation, never referenced by `deploy.ps1`, and the qualification gate still refusing | That the Job runs on ACA, the identity's RBAC grants, or that the projected `client_id` belongs to that identity |
| `cargo test --features server --test server_cli tenant_deployment` | Routes are opt-in and refused before a database is created | Any served lifecycle |
| `python tests/openapi_contract.py`, `python tests/admin_route_coverage.py` | Routes are registered with their OpenAPI contracts and are admin-only | Behavior |

Every test should pass. A refusal listed above is the expected result, not a
failure to work around. Passing suites do not complete the epic or any gate:
OD-002/004/005/006, ITEM-008, ITEM-021, MEAS-005/006 and GATE-005 stay open,
and auto-failover stays off by default.

### Control worker and ACA Job (offline only)

`vlite-server tenant-deployment work` is the control worker
(design-tenant-provisioning ITEM-022). **In production it always refuses
before it claims an operation or calls Azure.** The ARM adapter is wired,
but it is built only after every gate passes, and no qualification, quota
or budget proof exists (OD-002, OD-006).
This section explains what it does and what each result means.

```sh
vlite-server tenant-deployment work [--operation <id>] \
  --target-file target.json --object gateway/control-state.json --admin-dir /data/admin
```

It takes the same `--target-file`, `--object`, `--admin-dir` and optional
`--shared-key-file` flags and the same admin-directory lock as the other verbs
(`admin_dir_busy` while a server or another CLI holds it). One invocation does
the following, once:

0. **Release sweep (ITEM-024).** Before selecting anything, it finishes
   every sealed cancellation whose reservation release failed: a slot
   still reserved by an operation with the cancellation requested, no
   receipt and a sealed `Cancelled` terminal record. It reuses that seal
   and the same release an admin cancel runs, and it never seals, activates
   or touches another operation's slot. Each confirmed release attempts an
   audit event (`tenant_deployment_release_recovered`) and is listed in the
   output's `releases_recovered`, with whether that event was recorded. If a
   record is unreadable or a release stays
   unconfirmed, the invocation fails, the error names the operation and
   ends `; no operation was attempted`, and nothing is selected or
   claimed. Errors after any confirmed releases list their operation IDs
   and audit-recording status in the error message.
1. **Select.** With `--operation`, that operation. Otherwise, the unclaimed,
   unsealed `create` with the lowest operation ID whose reservation is held
   and not cancelled. If there is none, it prints
   `{"outcome": "idle", "releases_recovered": [...]}` and exits 0. An
   unknown ID answers `operation_not_found`. Without
   `--operation`, the worker lists the operations prefix once per invocation
   and stops at the first unreadable store or record (it fails closed rather
   than skipping it). Neither claims anything.
2. **Gates, in this order, before any claim or Azure call:**
   `operation_not_provisionable` (the reservation is gone or cancelled, or the
   operation is sealed); `actuator_fence_unproven` if another attempt already
   holds the claim (see step 3); then the writer profile, which refuses 409
   `profile_unsupported` for any kind other than `create` (reachable only
   with `--operation`), 503 `operation_input_unavailable` if the retained
   profile content is missing or corrupt, and 422 `operation_request_invalid`
   if the profile is legacy or does not match the strict writer-profile
   schema; then `qualification_missing`,
   `quota_unproven`, `quota_exceeded` and `budget_unapproved`. Only past
   every gate does the worker build the ARM adapter (ITEM-020's `ArmClient`,
   authenticating as the platform managed identity whose client ID the
   target file names; building it sends nothing). No production proof
   exists yet (OD-006), so production stops at the first gate that applies,
   at the latest at `qualification_missing`, and never builds the adapter
   or calls Azure. The same operation is selected again next time.
   To run a different one, pass `--operation`, or cancel the refused one.
3. **Claim.** A compare-and-swap on the operation journal records this
   attempt under a fresh `worker-<hex>` ID. If another attempt claimed the
   operation first, or wins the race, the worker refuses
   `actuator_fence_unproven` and does not try another operation. A stale or
   foreign claim is never taken over: takeover keeps refusing
   `actuator_fence_unproven` until a production `ActuatorStopped` proof exists
   (OD-002). There is no flag to force it. Once the claim succeeds, the
   operation stays claimed by that attempt whatever its outcome
   (`provisioned`, `failed` or `unknown`): no later invocation selects it,
   and an explicit `--operation` refuses `actuator_fence_unproven`.
4. **Attempt.** It runs one provision attempt. Each cloud action is journaled
   before it is sent and is settled only from an exact readback. The worker
   never retries an action, and apart from the step 0 release sweep it
   never writes the control state. Only the
   existing activation compare-and-swap publishes a route, and that needs
   readiness and delivery proofs that nothing produces yet.

A result is printed as JSON on stdout with exit 0. An error is printed as
`{"error": "<code>: ..."}` on stderr with exit 1, the same as the other verbs.
Once an operation is selected, the error also ends with
`; operation <id>, attempt <id>, outcome <outcome>`:

| Outcome | Exit | Meaning | What to do |
|---------|------|---------|------------|
| `idle` | 0 | Nothing to claim. | Nothing. |
| `provisioned` | 0 | Every profile resource was created or verified, and every step is settled. The tenant stays unrouted. | Offline fixtures only today. |
| `blocked` | 1 | A gate or the claim refused. Nothing was dispatched. | Resolve the named code, or wait for its owner decision. Rerunning does not clear `actuator_fence_unproven`. |
| `failed` | 1 | The attempt ended without provisioning (for example, Azure refused an action), and every journaled step is settled. It is not retryable: the operation stays claimed by this attempt until an `ActuatorStopped` proof exists (OD-002). | Read `status`, then escalate. A new attempt cannot take over this claim. |
| `unknown` | 1 | A step's outcome, or the claim's own write, is unknown. It is never reported as success, and `status` shows `uncertain_steps > 0`. | Do not rerun, and do not edit records. Escalate (see uncertain outcomes above). |

Each attempt on a selected operation appends a `tenant_deployment_attempt`
event (operation, attempt, outcome and refusal code) to the admin
directory's `audit.jsonl`. The operation journal in Blob storage is the
durable record.

**ACA Job (rendered and tested offline only).** `New-VLiteHaWorkerJobTemplate`
(`deploy/azure-container-apps/ha/templates.ps1`) renders a
`Microsoft.App/jobs` body for the worker. It is a pure, offline renderer.
`deploy.ps1` never calls it, and `Assert-VLiteHaQualification` still blocks
every HA deployment, so nothing in this repository creates or starts the Job.
Do not create it by hand. The rendered Job has:

- `triggerType` `Manual`, `parallelism` 1, `replicaCompletionCount` 1 and
  `replicaRetryLimit` 0. One execution starts one replica that runs `work`
  once. A retried replica would be a second attempt.
- `replicaTimeout` from `WorkerReplicaTimeoutSeconds` (default 1800, allowed
  300 to 14400).
- Its own user-assigned identity, `WorkerIdentityId`. This must differ from
  the gateway, writer and reader identities (compared case-insensitively).
- Exactly one secret, `control-target-g<SecretGeneration>`. It holds
  `ControlTarget` with only `client_id` replaced by the worker identity's
  client ID. That client ID must be a lowercase GUID and must differ,
  compared case-insensitively, from the `ControlTarget` and `ArchiveTarget`
  client IDs; the configuration has no other client IDs (serving identities'
  are not checked). The secret value is re-serialized from the validated
  object, so duplicate JSON keys cannot carry an unchecked value. Only
  managed identity is accepted, never a shared key. The secret is projected
  to `/run/vlite/control-target.json`. There is no admin token, seed, trust,
  archive or Azure Files mount, and no environment variables, ingress or
  probes.
- `--admin-dir /data` on an EmptyDir volume, so the audit log lasts for one
  execution only. Container logs carry the JSON result.

Inputs are `WorkerJobName`, `WorkerIdentityId`, `WorkerCpu`, `WorkerMemory`,
`ControlTarget` and, optionally, `WorkerReplicaTimeoutSeconds`. The offline
tests do not prove that the projected `client_id` belongs to
`WorkerIdentityId`, the identity's RBAC grants (none are rendered), or how ACA
runs the Job.

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

- Qualified (GATE-005) automatic failover, or an ACA-qualified,
  deployment-orchestrated completed fenced handoff; `failover execute` itself
  only prepares the transfer.
- Multi-writer operation or additional write capacity.
- Cross-region failover or geo-failing the authority.
- Power-loss-qualified Azure Files or production availability/durability SLOs.
- ACA gateway load and staleness measurements, or a completed ACA run of the
  Blob fencing measurement.
- Query routing of writes to readers, position tokens or read-your-writes overrides.
- Managed tenant provisioning, reader fleets, retirement or migration on ACA:
  the `tenant-deployment` surface records intent only, and its control worker
  refuses before any cloud call (see
  [managed tenant deployments](#managed-tenant-deployments-local-intent-only-not-aca-qualified)).
