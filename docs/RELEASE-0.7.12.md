# VLite 0.7.12 upgrade and rollback

VLite 0.7.12 adds **opt-in, unqualified dashboard sign-in through an HA
gateway** and the offline building blocks for **managed tenant deployments**
on Azure Container Apps: durable lifecycle intents with an admin REST, CLI and
read-only dashboard view, an Azure Resource Manager adapter, a control worker
and its ACA Job template. It builds on the experimental high-availability
features of [0.7.11](RELEASE-0.7.11.md). Check
[published releases](https://github.com/mahpate/Vlite/releases) for
downloadable artifacts; a Git tag alone does not publish them.

The database, admin-state, `memory-bindings-v1` and recovery-bundle formats are
unchanged from 0.7.11. No data migration is required. With none of the HA
commands, flags or files, a server runs exactly as 0.7.11 does. No benchmark
campaign was run for this patch; earlier measurements do not measure 0.7.12.

This is not production HA. Nothing in this release provisions, starts, stops
or routes an Azure resource in production: the control worker refuses at its
gates before it claims an operation or calls Azure, and every HA deployment
is still blocked by the qualification gate. Gateway dashboard sign-in has no
ACA run. Read
[Opt-in HA operations on Azure Container Apps](HA-ACA.md) before trying any of
it, and only in an isolated environment.

## What is new

### Dashboard sign-in through a gateway (opt-in, unqualified)

A `dashboard` block in the gateway configuration lets any gateway replica sign
in tenant editor and viewer accounts and serve the unchanged dashboard.
Accounts come from a read-only, tenant-only export of `users.json`, mounted as
a secret on gateways only. Sessions are stateless cookies sealed with a shared
session key, so every replica that holds the key accepts them and no session
store is needed. Each request is re-checked on the replica that serves it and
reaches the backend as a `user:<id>` principal carrying the standalone editor
or viewer scope.

- Super administrators, accounts without a tenant and password changes stay
  on the admin server.
- Sign-out clears only that browser's cookie. An account change takes effect
  after re-export and a restart of every replica; suspending the tenant pauses
  its sessions within S_ctl.
- The session key rotates in three rolling restarts without signing everyone
  out.
- Without the block, a gateway refuses sign-in and dashboard routes as before.

Upgrade every gateway-trusted backend before any gateway uses a `dashboard`
block: older backends refuse `user:` envelopes with 401 `envelope_invalid`.
See [Dashboard sign-in through a gateway](API.md#dashboard-sign-in-through-a-gateway-od-004-opt-in)
for the configuration, rollout, session ending and limits.

### Managed tenant deployments (intent only, off by default)

`vlite-server tenant-deployment create|readers|retire|status|resume|cancel|work`
and, with `--tenant-deployments-file`, the matching
`/v1/admin/tenant-deployments` and `/v1/admin/tenant-deployment-operations`
routes record durable, idempotent
lifecycle intents beside the control state. A `create` reserves an unrouted
tenant. Cancelling a reserved create seals it and releases the reservation;
cancelling an unreserved reader change or retirement withdraws it atomically.
`GET /v1/admin/tenant-deployment-operations` lists operations by ID, `limit`
(1-100, default 50) per page, continuing with `after` from the previous
`next_after`.
Status shows phases, generation tokens and journal counts, never keys,
profile contents or secrets. `migrate`, `rollback`, `release`,
`acknowledge-delivery` and `replace-credentials` stay unavailable.

Without the flag, authenticated requests answer 409
`tenant_deployments_not_configured`. See
[Managed tenant deployments](HA-ACA.md#managed-tenant-deployments-local-intent-only-not-aca-qualified)
and the [API reference](API.md#managed-tenant-deployments-intent-only-opt-in-off-by-default).

### Control worker, ARM adapter and ACA Job (offline only)

- **Control worker.** `vlite-server tenant-deployment work` selects one
  reserved `create`, runs its gates, claims it with a compare-and-swap and runs
  one provision attempt. In production it always refuses before the claim, at
  the latest with `qualification_missing`. Each attempt on a selected
  operation is audited as `tenant_deployment_attempt`.
- **ARM adapter.** An Azure Resource Manager client for Container Apps,
  journaling every action before it is sent and settling it only from an exact
  readback. It never resends an unknown action or adopts a foreign resource.
  It is exercised only against offline fakes; production never constructs it.
- **ACA Job template.** `New-VLiteHaWorkerJobTemplate` renders a manual,
  single-attempt `Microsoft.App/jobs` body with its own identity and only the
  control-target secret. `deploy.ps1` never calls it.

### Reader pin isolation (offline only)

The reader option `isolated_pins` (default `false`) pins with per-replica
objects under `<prefix>reader-pins/` instead of rewriting the set record.
`New-VLiteHaReaderStorageGrants` builds the proposed per-tenant reader role
and conditional assignments without calling Azure. Neither Azure RBAC/ABAC
scoping nor mount enforcement is qualified, and provisioned reader fleets
still refuse `pin_scope_unproven`.

### Dashboard

- **Tenant deployments (read only).** Super administrators signed in to the
  server directly see **Administration → Users & access → Tenant
  deployments**, 20 operations at a time. The panel submits nothing and stays
  locked with open development access and through a gateway.
- **Automatic failover in open development access.** The switch is shown read
  only with the reason, instead of offering a change the server refuses.

### Measurement harnesses

The offline MEAS-004 harness gains diagnostics and dedicated-thread pacing for
high request rates, and MEAS-005 gains offline modes. They run nothing against
production and do not change any measurement result.

## Upgrade

Verify release checksums and back up the database, private bindings file and
admin state. Stop 0.7.11 before starting 0.7.12; keep one writer per database
file. Preserve existing authentication, durability, proxy, storage and memory
settings. No HA feature is enabled by the upgrade. Keep a separate dashboard
image at the same version as the API. Before adding a gateway `dashboard`
block, upgrade every gateway-trusted backend.

When upgrading from 0.7.8 or earlier, also read the
[0.7.9 upgrade notes](RELEASE-0.7.9.md#upgrade).

## Rollback

Remove any gateway `dashboard` block, turn the automatic-failover switch off
with `vlite-server failover policy disable` and stop every supervisor. Then
stop 0.7.12 before starting 0.7.11. Formats are compatible; no reverse
migration is required.

Once a tenant-deployment operation has reserved or been withdrawn, the
control-state object carries a `lifecycle` or `withdrawn` field. Older strict
readers then fail closed on it rather than ignoring it, so after using
managed tenant deployments roll the binary forward instead. Never strip those
fields, reservations or operation records to make an older binary start. For
earlier rollback targets, follow the
[0.7.11 rollback guidance](RELEASE-0.7.11.md#rollback).

## Compatibility and known limits

The [0.7.11 limits](RELEASE-0.7.11.md#compatibility-and-known-limits) apply.
In addition:

- Gateway dashboard sign-in is unqualified: no multi-process, Azurite or ACA
  run covers it, sign-out is per browser, there is no server-side session
  revocation and the sign-in throttle is per replica.
- Managed tenant deployments record intent only. No production ARM adapter or
  qualification exists, no route is published and no credential is delivered.
  Existing-tenant migration is not implemented.
- The dashboard shows tenant deployments but cannot submit, resume or cancel
  them; use the CLI or REST routes.
- No ACA-qualified deployment. Gateway load and control-state measurements
  have not qualified, and the writer-fencing measurement has not produced a
  usable ACA result; see the
  [HA measurement summary](HA-ACA.md#what-has-been-measured).
