---
name: vlite-ops
description: Operate VLite safely with file locking, read-only snapshots, backups, verification, copy-out compaction, recovery and monitoring. Use when asked to back up or repair VLite, troubleshoot Locked or Corrupt errors, share a file among readers, or inspect operational metrics.
---

# Operate VLite safely

## Confirm ownership and locking

- Normal open holds **one exclusive writer lock**. It excludes other writers
  **and read-only openers**, even when the owner is currently only reading.
- Rust `Database::open_read_only(path)`, Python
  `native.open(path, read_only=True)`, and Node
  `native.open(path, {readOnly: true})` acquire shared locks. Many readers may
  share a file across processes, but no writer may coexist with them.
- The Python `read_only` argument is available in current repository source,
  not in the published [PyPI 0.3.0 package](https://pypi.org/project/vlite-embedded/).
  Install that release with `python -m pip install vlite-embedded`; build current
  source when you need the newer option.
- Each reader sees the snapshot at open time. Close **all** readers to let a
  writer commit, close the writer, then reopen readers to see new commits.
  This is not live refresh or concurrent writer/read-snapshot MVCC.
- Rust read methods support concurrent shared references. C ABI calls serialize
  within one process; for parallel Python reads, use spawned processes that each
  load/open their own library and read-only handle. Never reuse forked handles.
- Never bypass OS locks or rename over an active file. Do not assume an older
  CLI's `--read-only` guard opens a shared handle: the documented shell guard
  may still use exclusive ownership; check the installed implementation.

## Backup and verify before changing anything

1. Identify the active file (including a server restore selection), encryption
   status, expected format, free space and every owner.
2. Stop writers/readers/server and close handles. Maintenance is offline and
   takes an exclusive source lock; a read-only file descriptor is not a shared
   reader lock.
3. For healthy unencrypted data, use the actual positional subcommands:

   ```powershell
   vlite-maintain inspect data.vlite
   vlite-maintain backup data.vlite backup-new.vlite
   vlite-maintain verify backup-new.vlite
   New-Item -ItemType Directory -Path drill-new-directory | Out-Null
   vlite-maintain drill backup-new.vlite drill-new-directory
   ```

4. Keep access-controlled off-host backups. Test restore to a **new** path and
   verify application records/search results. CRCs alone do not prove coverage.
5. Never overwrite the only copy. Outputs use no-clobber publication. For
   encrypted files use keyed APIs; this CLI has no key flags.

See [maintenance and errors](references/maintenance.md) for supported commands,
encryption and recovery restrictions. Repository `docs/TOOLS.md` covers the
shell/import/tune/quality tools; `src/bin/vlite-maintain.rs` defines this CLI.

## Compact, upgrade or repair deliberately

- `vlite-maintain compact SOURCE DESTINATION` is **copy-out**, not in-place.
  Verify the new file/application behavior, update the configured path and retain
  the original for rollback. Budget both files plus RAM. Rebuild sidecars and
  reacquire conditional tokens as needed.
- `vlite-maintain upgrade SOURCE DESTINATION` enables the installed tool's
  documented target format. Inspect output; never assume it enables all of V5.
- There is **no `vlite-maintain repair` subcommand**. An incomplete final frame
  can only be discarded through explicit engine repair APIs after preserving a
  closed forensic copy and accepting potential data loss. Complete corrupt
  frames are never repaired by tail truncation.
- On `CommitIndeterminate`, `RepairIndeterminate` or publication uncertainty,
  stop, inspect and reconcile before retrying. Failure does not prove a mutation
  was absent. Never automatically reset/recreate a database.

## Derived indexes and secrets

ANN snapshots are rebuildable caches, **not backups**. Server `--ann-snapshot`
uses `<active-database-path>.ann`, loading after open and saving on graceful
shutdown. Commits/copy-out maintenance can invalidate it. Missing/stale caches
call for rebuilding, not modifying the primary. See `docs/ANN-SNAPSHOTS.md`.

Snapshots/disk ANN contain plaintext vector information and are refused for
encrypted files (`SidecarUnavailable`); build HNSW in memory instead. Never
attach files/sidecars to public logs/issues. Encryption keys are 32-byte secrets
or passphrases supplied through documented APIs, never hard-coded, logged or
committed. Losing the key loses the data. Keep key access for backups and rotate
to a verified new destination. Encryption protects frame contents, not all
header/file/access-pattern metadata.

## Observe, then diagnose

- Check authenticated `/metrics` for operational counters/latencies/readiness;
  default is JSON, not Prometheus text. Current servers may also expose
  `/metrics?format=prometheus`; verify the deployed release.
- Use the dashboard for collection/index state and failures. Its evolving
  **Observability** tab adds metrics/live logs via `/v1/admin/metrics/summary`
  and `/v1/admin/logs`. Confirm availability/admin authorization in your build;
  do not assume logs are a durable audit trail.
- Correlate request IDs, error codes, disk capacity, write failures, legitimate
  lock owners, dimensions, ANN readiness and file/sidecar identity. Redact keys,
  tokens, source text and vectors from troubleshooting reports.
- Prefer non-destructive inspection. Report observed errors, mitigations,
  verification and any accepted data-loss risk.
