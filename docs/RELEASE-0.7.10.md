# VLite 0.7.10 upgrade and rollback

VLite 0.7.10 is the first published release of the **experimental, opt-in
high-availability building blocks for Azure Container Apps** described in the
[0.7.9 notes](RELEASE-0.7.9.md). The `v0.7.9` tag exists, but its release build
failed Windows lint and a macOS compaction test before publication. There are
no 0.7.9 release assets, container images, Python wheels or npm packages.

0.7.10 contains every 0.7.9 change plus two fixes. The gateway measurement
harness builds without an unused-`mut` warning on non-Unix targets. When an
operator polls a compaction job, its terminal outcome is now visible only
after the corresponding serving fence has reopened or entered recovery.
The database, admin-state, `memory-bindings-v1` and
recovery-bundle formats are unchanged from 0.7.8. No data migration is
required. No benchmark campaign was run for this patch; earlier measurements
do not measure 0.7.10.

This is not production HA. There is one writer per database, no automatic
failover and no qualified RPO, RTO or availability SLO. Read
[Opt-in HA operations on Azure Container Apps](HA-ACA.md) before trying any of
it, and only in an isolated environment.

## What is new

Everything listed in [What is new in 0.7.9](RELEASE-0.7.9.md#what-is-new):
serving modes and `GET /v1/admin/serve-status`, reader replicas from the Blob
archive, the stateless gateway, writer authority with epoch fencing and
operator-run failover, and the HA deployment tooling. With none of the new
commands, flags or files, a server runs exactly as 0.7.8 does.

## Upgrade

Upgrade from 0.7.8 as described in the [0.7.9 upgrade notes](RELEASE-0.7.9.md#upgrade),
using 0.7.10 artifacts. Verify release checksums and back up the database,
private bindings file and admin state. Stop the old server before starting
0.7.10; keep one writer per database file. No HA feature is enabled by the
upgrade. Keep a separate dashboard image at the same version as the API.

## Rollback

Stop 0.7.10 before starting 0.7.8; there are no 0.7.9 artifacts to roll back
to. Follow the [0.7.9 rollback guidance](RELEASE-0.7.9.md#rollback): formats
are compatible, but 0.7.8 cannot read writer-authority records, HA archive
manifests, control state or reader-apply configuration, so roll back only to a
standalone database.

## Compatibility and known limits

The [0.7.9 limits](RELEASE-0.7.9.md#compatibility-and-known-limits) all apply.
In short: no automatic failover, one writer per database, no ACA-qualified
deployment (gateway load and staleness measurements have not run on ACA, and
the Blob fencing measurement has not completed there), Azure Files remains
experimental, and the dashboard has no HA view.
