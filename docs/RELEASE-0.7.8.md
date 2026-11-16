# VLite 0.7.8 upgrade and rollback

VLite 0.7.8 adds **memory-aware Azure Blob backup and recovery**. A server with
agent-memory bindings can now back up, drill and restore its database instead
of being refused with `memory_reservations_present`.

The database format and `memory-bindings-v1` file format are unchanged. No data
migration is required. There is no new route or request field; recovery reports
add memory counts and the offline restore reports its bindings-file path.
Backups carrying bindings use a new recovery-bundle schema, described below.
No benchmark campaign was run for this patch; earlier measurements do not
measure 0.7.8.

## What is new

### Capture bindings with the database

A backup captures the binding set in force under the same exclusive write lock
as the database snapshot. A concurrent binding reload cannot pair one
generation's database with another generation's grants.

When bindings are present, the recovery bundle uses
`vlite.recovery-bundle/2` and carries their strict `memory-bindings-v1`
representation. Backups without a binding set keep the existing `/1` schema.
Orphaned binding records remain reserved in a restored database without
granting access, even if the source had no bindings file.

### Validate memory before publishing a restore

A drill and both restore paths run the startup binding checks against the
restored copy. Missing graphs or evidence collections, invalid records and
incompatible binding generations fail recovery rather than silently dropping
memory protection. A failed offline restore publishes no output.

Reports give binding, active, retired, principal-to-rekey and orphan-record
counts, not binding or principal names. A malformed memory section returns
`cloud_restore_memory_invalid`; a failed startup check returns
`cloud_restore_memory_failed`. The REST errors are 409, and a failed drill
reports `ok: false`.

### Recover onto a new host without restoring old grants

Offline `vlite-server cloud-restore` writes the captured set to
`memory-bindings.json` beside `database.vlite` and `admin/`. The file is
owner-only on Linux and macOS; on Windows, use an operator-private output
parent because the output inherits its access list.

Credentials and sessions are still not restored. Before serving memory,
re-issue credentials, replace each binding's principal IDs, raise its
`generation` and preserve or narrow `raw_access`. Then start with
`--memory-bindings-file`. Without the file, the restored memory remains
reserved with no grant. REST restore validates memory and reports its counts
but writes no bindings file; provision that file separately.

See the [recovery runbook](DOCKER-OPERATIONS.md#cloud-backups-to-azure-blob),
[API details](API.md#cloud-backups-to-azure-blob-opt-in-off-by-default) and
[security boundaries](SECURITY.md#cloud-backups-azure-blob).

## Upgrade

Verify release checksums and back up the database, private bindings file and
admin state. Stop the old server before starting 0.7.8; keep one writer per
database file. Preserve existing authentication, durability, proxy, storage
and memory settings. Existing bindings need no generation change for the
upgrade itself.

Upgrade all tools that read cloud backup sets before relying on memory-aware
backups. Servers and restore tools older than 0.7.8 reject `/2` bundles; they
cannot drill or restore these sets. Existing `/1` sets remain readable.

Cloud backups remain off until configured, and the upgrade does not enable
them, retention or compaction. The existing dashboard Cloud backup card drives
the feature without a UI change. The Windows and Ubuntu setup scripts now
default to the current release images; an explicit version still overrides
that default. Keep a separate dashboard image at the same version as the API.

When upgrading from older releases, also read the
[0.7.7 notes](RELEASE-0.7.7.md#upgrade) and
[0.7.6 cloud-backup prerequisites](RELEASE-0.7.6.md#upgrade).

## Rollback

Stop 0.7.8 before starting 0.7.7. The existing live database, binding file and
admin-state formats are compatible; no reverse migration is required.
However, 0.7.7 refuses cloud backup on a server with memory reservations and
cannot read the new `/2` recovery bundles. Keep 0.7.8 restore tooling for those
sets; do not relabel or strip a bundle to make an older tool accept it.

Rolling back the server does not undo a restore or its re-issued credentials.
Keep the restored database, updated bindings and corresponding admin state
together. For earlier rollback targets, follow the
[0.7.7 rollback guidance](RELEASE-0.7.7.md#rollback).

## Compatibility and known limits

- Memory-aware backup does not add HA, automatic failover, continuous log
  archiving or same-lineage standby seeding.
- Encrypted databases, in-memory servers, databases over 1 GiB and unrecognized
  admin-directory contents retain their existing cloud-backup refusals.
- File compaction still refuses memory reservations. Its filesystem
  qualification and all other safety gates are unchanged.
- Restoring an older backup can bring back memories and evidence forgotten or
  erased after capture. Apply retention, legal holds and key-retirement policy
  to every backup copy.
- This release adds no real-Azure recovery-time, power-loss or HA qualification.
  Azure Files remains experimental, not power-loss or cross-host writer-lock
  qualified. Blob replication does not provide writer fencing, and the
  coordination container must not fail over across regions.
