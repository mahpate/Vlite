# VLite 0.7.4 upgrade and rollback

VLite 0.7.4 adds read-only preflight and status checks around MemoryAI's
restart-free grant reload, and requires exact-commit full CI before publishing
release images, GitHub assets or Python wheels.

The database format and binding-file schema are unchanged. No data migration
is required. No benchmark campaign was run for this patch release; earlier
measurements do not measure 0.7.4.

## What is new

### Grant-change preflight and status

Administrators can call `POST /v1/admin/memory/bindings/preflight` to check the
configured private bindings file without applying it. Inspect the response's
`outcome`, not just HTTP 200: `reload` or `unchanged` passes the current checks.
Preflight is advisory, does not take the reload lock, and cannot guarantee that
a later reload succeeds. Reload always rereads and validates the file.

After a successful reload, `GET /v1/admin/memory/bindings/status` reports whether
the file agrees with the served grants and whether requests using older grants
have drained. Status is read-only and takes no database or reload lock; its
snapshot can lag a concurrent reload by one call. Poll for `file: "agrees"` and
`drained: true`. A failed reload is not a revocation.

The dashboard Administration workspace exposes preflight, reload and status
together. Both new endpoints require administrator authorization and report
bounded, sanitized results rather than binding names, paths or private grants.
See the [memory guide](MEMORY.md#change-grants-live) and
[API contract](API.md#change-grants-without-a-restart).

### Exact-commit publication guard

Publication requires a successful full `ci.yml` run at the tagged commit,
including Docker and the complete required matrix. Older, partial, foreign
or pull-request CI does not qualify. This guards image digest pushes as well
as image tags, GitHub release assets and PyPI uploads.

PyPI additionally verifies the release manifest, checksums and each wheel's
source commit. Every distinct source must have qualifying CI. GitHub API
retries and polling are bounded and fail closed. Branch release dry runs
report qualification without publishing. See
[release CI guard](INSTALL.md#release-ci-guard) for the required ordering.

## Upgrade

Verify release checksums and back up the database, private bindings file and
admin state. Stop the old server before starting 0.7.4; keep one writer per
database file. Preserve existing durability, authentication, proxy and storage
settings. There is no format conversion or binding-generation change solely
for this upgrade.

Existing clients continue to work. Clients using the new admin endpoints must
wait until the server is upgraded. Grants-only reload remains limited to
bindings already served by that process; first-time enablement and structural
binding changes still require a restart.

When upgrading from 0.7.2 or earlier, also read the
[0.7.3 compatibility guidance](RELEASE-0.7.3.md), especially before introducing
read-only shared scopes.

## Rollback

Stop 0.7.4 before starting 0.7.3. The unchanged database format and binding
schema require no migration. Stop clients from calling preflight/status on
the older server; use its existing reload procedure instead.

For rollback to 0.7.2 or earlier, follow the
[0.7.3 rollback instructions](RELEASE-0.7.3.md#rollback). Those releases
refuse bindings files containing `read_scopes`; never turn read-only grants
into read/write grants without reviewing the intended access.

## Compatibility and known limits

- Memory remains opt-in and REST-only. Binding lifecycle operations, file
  watching, gRPC memory and gRPC live reload are not added.
- Preflight and status do not edit the file, mutate the database, apply
  grants or replace reload validation. Status alone cannot repair a failed
  reload.
- Existing receipt lookup and idle-binding cleanup limitations are unchanged.
- This release does not add managed followers, multi-writer operation,
  automatic failover or a larger request-concurrency limit.
- Azure Files remains an experimental deployment option, not power-loss
  or cross-host writer-lock qualification.
