# VLite 0.7.7 upgrade and rollback

VLite 0.7.7 is a dashboard patch. It adds a **Cloud backup** card for the Azure
Blob backups that arrived in 0.7.6, and it lets the MemoryAI **Remember** form
attach an evidence record or merge a source into an existing memory.

The server's routes, requests and responses, the database format and the
binding-file schema are unchanged. No data migration is required. No benchmark
campaign was run for this patch release; earlier measurements do not measure
0.7.7.

## What is new

### Cloud backup card

A super administrator can now open **Administration → Cloud backup (Azure
Blob)** instead of calling `/v1/admin/cloud-backup` by hand. The card:

- shows the current status and, in plain words, what needs attention before a
  backup can run;
- saves the settings: **Back up automatically**, the storage account,
  container, an optional separate coordination container, folder, optional
  managed identity client ID, how often to back up, how often to test a
  restore, and the clean-up settings;
- runs **Back up now** and **Clean up now**;
- loads the stored backups, then tests one, restores it as a new copy, or holds
  it and later releases the hold.

No password or key is entered or shown: the server signs in to Azure with its
managed identity. Saving settings uploads and deletes nothing, and nothing is
uploaded until you turn on automatic backups or select **Back up now**. As in
0.7.6, nothing is deleted unless **Delete old backups automatically** is on,
and even then the server keeps held backups, the newest verified backup and any
backup being restored. Opening the dashboard does not read cloud backup state;
select **Refresh status**.

See the
[dashboard walkthrough](CONSOLE-ADMIN.md#dashboard-administration-walkthrough)
and the
[Azure setup and restore steps](DOCKER-OPERATIONS.md#cloud-backups-to-azure-blob).

### MemoryAI: evidence and merge in Remember

The **Remember** form in the MemoryAI workspace has two new optional parts. Both
use features the memory REST API already had.

- **Evidence record number** attaches one existing record from the binding's
  evidence collection. It needs a token with raw access, and the dashboard does
  not create the record. Attaching transfers the record to the memory, so
  forgetting the memory deletes the record too; do not attach a record you
  still need.
- **Add this source to an existing memory** merges provenance into a memory
  that already exists. It needs the same memory ID, kind and content, a new
  source ID and the memory's current revision from a recent recall. A wrong
  revision is refused, so recall again before retrying.

After the first attached evidence reference the database cannot roll back to
0.7.0 (see [Rollback](#rollback)). See the
[MemoryAI guide](CONSOLE.md#memoryai) and
[evidence references](MEMORY.md#evidence-references-and-raw-access).

### Tests and website

The scheduled-compaction test now retries a write that a starting run refuses
with `503 maintenance_in_progress`. The website's dashboard tours were
recaptured to show the new card and the Remember form. Neither changes the
server.

## Upgrade

Verify release checksums and back up the database, private bindings file and
admin state. Stop the old server before starting 0.7.7; keep one writer per
database file. Preserve existing durability, authentication, proxy and storage
settings. There is no format conversion or binding-generation change for this
upgrade.

The server serves the dashboard from its own binary, so upgrading the server
brings both dashboard changes. If you also run the separate dashboard image
(`ghcr.io/mahpate/vlite-dashboard:<version>`), upgrade it together with the
server and use the same version for both; see
[Docker operations](DOCKER-OPERATIONS.md#upgrade).

Existing clients work unchanged, and cloud backups stay off until a super
administrator turns them on. Before enabling them, read the
[0.7.6 upgrade notes](RELEASE-0.7.6.md#upgrade): the app needs a managed
identity with `Storage Blob Data Contributor` on the two containers, and only
server-written files may sit in `VLITE_ADMIN_DIR`.

When upgrading from 0.7.2 or earlier, also read the
[0.7.3 compatibility guidance](RELEASE-0.7.3.md), especially before introducing
read-only shared scopes.

## Rollback

Stop 0.7.7 before starting 0.7.6. The two releases share the database format,
the binding file and the `compaction/` and `cloud-backup/` folders in the admin
directory, so 0.7.6 starts on the same files with no migration. Cloud backup
settings saved from the new card stay in place, because the card uses the same
admin routes as 0.7.6.

To go back further than 0.7.6, follow the
[0.7.6 rollback notes](RELEASE-0.7.6.md#rollback), in particular the warning
about a compaction that has already switched files. For 0.7.2 or earlier,
follow the [0.7.3 rollback instructions](RELEASE-0.7.3.md#rollback).

Once a memory holds an evidence reference, whichever client attached it, do not
roll back to 0.7.0; see the [0.7.1 notes](RELEASE-0.7.1.md#rollback).

## Compatibility and known limits

- The card drives the 0.7.6 cloud backup feature and has its limits; see the
  [0.7.6 known limits](RELEASE-0.7.6.md#compatibility-and-known-limits). The
  card is covered by dashboard unit tests and a browser test against a stand-in
  for the admin routes. Nothing in them reaches Azure, and this release adds no
  live Azure measurement.
- The Remember form attaches an existing evidence record; it cannot create
  one. Creating evidence still needs a raw-access client.
- File compaction is unchanged and remains experimental, qualified on one local
  ext4 profile with process kills only.
- Azure Files remains an experimental deployment option, not power-loss or
  cross-host writer-lock qualified.
