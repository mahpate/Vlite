# VLite 0.7.9 upgrade and rollback

VLite 0.7.9 adds **experimental, opt-in high-availability building blocks for
Azure Container Apps**: a stateless gateway, read-only reader replicas fed
from the Azure Blob archive, writer authority with epoch fencing, and an
operator-run failover procedure. With none of the new commands, flags or
files, a server runs exactly as 0.7.8 does: no new headers, fields, reads,
errors or startup requirements.

The database, admin-state, `memory-bindings-v1` and recovery-bundle formats are
unchanged. No data migration is required. One administrator route,
`GET /v1/admin/serve-status`, is added. No benchmark campaign was run for
this patch; earlier measurements do not measure 0.7.9.

This is not production HA. There is one writer per database, no automatic
failover and no qualified RPO, RTO or availability SLO. The HA deployment
scripts stop at their qualification gate before rollout. Read
[Opt-in HA operations on Azure Container Apps](HA-ACA.md) before trying any of
it, and only in an isolated environment.

## What is new

### Serving modes and serve status

`--serve-mode standalone|writer|reader` selects a role; `standalone` is the
default. A reader serves a read-only follower file.
`GET /v1/admin/serve-status` reports the mode, drain and read-only state,
lineage, binding generation, sequence, claimed epoch, archive progress and
reader freshness. It works independently of `/metrics`, including with memory
bindings. See [serve status](API.md#serve-status-and-reader-freshness).

### Reader replicas from the Blob archive

A reader started with `--reader-apply-file` enrolls on a verified archive set
and applies only verified history in two private follower generations. It
holds a whole-set retention pin and needs a capacity cap (at most 1 GiB in
this release), a staleness bound and a poll interval; none has a default. A
stale or failed reader fails `/ready` and authorized reads with 503
`reader_stale` or `reader_apply_failed`. Authenticated responses from a
following reader carry `X-VLite-Lineage`, `X-VLite-Binding-Generation`,
`X-VLite-Applied-Age-Ms` and `X-VLite-Applied-Sequence`, plus `X-VLite-Epoch`
when it follows a writer authority. These provide no read-your-writes
guarantee.

### Stateless gateway

`vlite-server gateway` opens no database. It authenticates tenant credentials
against a shared control-state object in Azure Blob, applies the existing
route-to-permission map and a per-replica share of each credential's rate
limit, and forwards to the tenant's backend with a signed principal envelope.
A backend started with `--gateway-trust-file` verifies that envelope and
checks authorization again. `vlite-server gateway-keygen` and
`vlite-server control-state` manage signing keys and control-state
generations. Writes always go to the writer; reads go to the writer unless a
dedicated tenant opts into reader routing. No role or permission is added. See
[gateway mode](API.md#gateway-mode-and-gateway-trusted-backends-experimental-opt-in-off-by-default).

### Writer authority, fencing and manual failover

`vlite-server authority init|show` creates and inspects a writer-authority
record in Azure Blob. A writer started with `--serve-mode writer
--authority-target-file` claims that authority before listening, rechecks it
around writes and periodically (`--authority-recheck-ms`), binds its epoch
into every archived set and checkpoint and advances the archive head by
conditional write. When it observes a replacement, it fences itself, drains
and exits. Missing or conflicting authority fails closed; nothing reclaims or
elects a writer automatically. A gateway-trusted writer, which cannot use an
admin directory, configures its archive with `--archive-policy-file`.

`vlite-server failover report|execute` prints a loss report from the
archived position and, only after the operator acknowledges that exact report
and confirms zero old writer replicas, advances the epoch. The replacement
writer then restores exactly the verified archived base before opening its
database. Commits after that base are lost and are disclosed as such.

### HA deployment tooling

HA app templates with mocked tests, and disposable measurement harnesses for
temporary, tagged Azure resource groups, live under
[`deploy/azure-container-apps/ha/`](../deploy/azure-container-apps/ha/README.md).
They do not change the singleton deployment.

## Upgrade

Verify release checksums and back up the database, private bindings file and
admin state. Stop the old server before starting 0.7.9; keep one writer per
database file. Preserve existing authentication, durability, proxy, storage
and memory settings.

No HA feature is enabled by the upgrade. A database file literally named
`gateway`, `gateway-keygen`, `gateway-measure`, `control-state`, `authority`
or `failover` must now be given with a `./` prefix, as `cloud-sets` and
`cloud-restore` already were. Keep a separate dashboard image at the same
version as the API; the dashboard has no HA view.

When upgrading from older releases, also read the
[0.7.8 notes](RELEASE-0.7.8.md#upgrade).

## Rollback

Stop 0.7.9 before starting 0.7.8. Database, bindings, admin-state and `/1` and
`/2` recovery-bundle formats are compatible; no reverse migration is required.
0.7.8 cannot read writer-authority records, HA archive manifests, control
state or reader-apply configuration. After any HA trial, roll back only to a
standalone database, and do not reuse an authority-bound archive or
superseded database with 0.7.8. For earlier rollback targets, follow the
[0.7.8 rollback guidance](RELEASE-0.7.8.md#rollback).

## Compatibility and known limits

- No automatic failover. `failover execute` prepares a transfer; it does not
  start, route to or verify a replacement writer.
- One writer per database. Gateway and reader replicas add read capacity and
  isolation, not write capacity or writer HA.
- No ACA-qualified deployment. Measurement runs on temporary Azure resources
  are proposal evidence only, not SLOs or capacity qualification. Gateway
  load and staleness measurements have not run on ACA, and the Blob fencing
  measurement has not completed there.
- Azure Files remains experimental, not power-loss or cross-host writer-lock
  qualified. Cross-region failover and geo-failover of the authority are not
  provided.
- Encrypted databases cannot be followed by readers. gRPC is refused on
  readers, authority writers and gateway-trusted backends.
- On a gateway-trusted backend, admin routes such as serve-status are
  unavailable; use `authority show`, `/ready` and the freshness headers.
- Moving a tenant between deployments has no operator command yet.
- The dashboard has no HA view.
