# VLite 0.7.6 upgrade and rollback

VLite 0.7.6 adds two server features, both off by default: file compaction,
which a super administrator can run from the dashboard, and cloud backups to
Azure Blob Storage with an isolated, rekey-only restore.

The database format and binding-file schema are unchanged. No data migration
is required. No benchmark campaign was run for this patch release; earlier
measurements do not measure 0.7.6.

## What is new

### File compaction (experimental)

A single-file database keeps old pages after updates and deletes, so the file
grows. File compaction writes a compact copy, checks it and switches the
server to it. In the dashboard, open **Administration → Backup & recovery →
File compaction** and choose **Compact now**, or turn on the daily automatic
window (UTC, off by default). The same actions are available through
`/v1/admin/compaction` for super administrators.

- It runs only on the managed `vlite-server` with `VLITE_ADMIN_DIR`, a
  persisted database and no gRPC listener. The only qualified profile is
  `linux-local-ext4-v1`: Linux, the database and admin directory on the same
  local ext4 filesystem, files up to 1 GiB and about four times the file size
  free. Anything else, including Azure Files, native Windows and macOS,
  encrypted files and open mode, is refused with a plain reason.
- While a job runs, reads and writes answer `503 maintenance_in_progress`
  with `Retry-After: 5`. The recorded 1.02 GB run took 8.6 seconds in total.
- Records, graphs, credentials, accounts and the lineage are kept. Change
  history from before the run is dropped (`cursor_compacted`), and record
  ETags and graph revisions can advance, so both are acknowledged before a
  run. The original file is kept as a recovery copy.
- Qualification used process kills on one host; there is no power-loss
  evidence. Run `tests/compaction_qualification.py` on your own host first.

See [API.md](API.md#file-compaction-experimental-off-by-default) and
[running it safely](DOCKER-OPERATIONS.md#file-compaction).

### Cloud backups to Azure Blob

A super administrator saves a policy with `PUT /v1/admin/cloud-backup`. The
server then publishes verified backup sets of the database and a secret-free
recovery bundle (tenants, account names and roles, the credential inventory)
to two Blob containers, using the container app's managed identity. Each set
is hashed end to end, read back before it is published and can be drilled
(downloaded, restored to scratch and queried) without touching the live
database. Retention is off by default and never removes the newest verified
set, pinned sets or sets inside `keep_sets` (at least 3) or `keep_days` (at
least 30).

Restore is rekey-only. `POST .../sets/{id}/restore` writes a new-lineage
`restore-*.vlite` copy for the existing restore selection, and the offline
`vlite-server cloud-sets` and `vlite-server cloud-restore` commands rebuild a
database and admin directory on a new host. Restored accounts are disabled,
a fresh super administrator is the only working sign-in, and credentials,
sessions and webhooks must be issued again.

There is no dashboard card yet. See
[API.md](API.md#cloud-backups-to-azure-blob-opt-in-off-by-default) and the
[Azure setup and restore steps](DOCKER-OPERATIONS.md#cloud-backups-to-azure-blob).

## Upgrade

Verify release checksums and back up the database, private bindings file and
admin state. Stop the old server before starting 0.7.6; keep one writer per
database file. Preserve existing durability, authentication, proxy and storage
settings. There is no format conversion or binding-generation change for this
upgrade.

Existing clients and the dashboard work unchanged, and both features stay off
until a super administrator turns them on. Before enabling cloud backups, give
the app a managed identity with `Storage Blob Data Contributor` on the two
containers, and keep only server-written files in `VLITE_ADMIN_DIR`; a token or
TLS file there makes runs answer `409 recovery_config_unrecognized`.

When upgrading from 0.7.2 or earlier, also read the
[0.7.3 compatibility guidance](RELEASE-0.7.3.md), especially before introducing
read-only shared scopes.

## Rollback

Stop 0.7.6 before starting 0.7.5. Do not roll back while a compaction job is
running or the server reports `recovery_required`.

- If file compaction never switched the server to a compacted file, 0.7.5
  starts on the same files with no migration. It ignores the
  `compaction/` and `cloud-backup/` folders in the admin directory, so
  scheduled cloud backups stop. Published sets stay in Blob Storage, and
  0.7.6 can list and restore them again.
- After a compaction has switched files, do not start 0.7.5 on that
  installation. It does not read the compaction selection that chooses the
  startup file, so it could open the original file and lose later writes.
  To go back, restore a backup into a separate installation with an empty
  admin directory, as described in
  [DOCKER-OPERATIONS.md](DOCKER-OPERATIONS.md#file-compaction). Writes made
  after that backup are not in it.

For rollback to 0.7.2 or earlier, follow the
[0.7.3 rollback instructions](RELEASE-0.7.3.md#rollback).

## Compatibility and known limits

- Compaction is experimental and qualified on one local ext4 profile with
  process kills only. Encrypted files, memory bindings, webhook subscriptions
  and imports in progress are refused.
- Cloud backup evidence comes from fakes and the Azurite emulator; real Azure
  role assignments, immutability policies, power loss and recovery time are
  not yet measured. Databases over 1 GiB, encrypted databases and servers with
  memory reservations are refused.
- One identity both uploads and deletes, and sets are hashed, not signed:
  authenticity rests on who may write the containers.
- Same-lineage standby seeding (`{"mode": "seed"}`) is refused with
  `cloud_seed_mode_deferred`. This release does not add managed followers,
  automatic failover or fencing.
- Azure Files remains an experimental deployment option, not power-loss
  or cross-host writer-lock qualification.
