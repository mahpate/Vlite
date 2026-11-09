# VLite 0.7.3 upgrade and rollback

VLite 0.7.3 hardens opt-in agent memory with per-scope read grants and bounded
cleanup of expired write receipts, and lets administrators reload grants for
already-served bindings without restarting the server.

The database format is unchanged; no data migration is required. Existing
bindings files continue to work until they use the optional `read_scopes` field.
No benchmark campaign was run for this minor release.

## What is new

### Per-scope read grants

A credential may list `read_scopes` alongside its read/write `scopes`. A
read-only scope can use `recall` (and the currently unavailable `facts/query`
surface); it cannot write, look up receipts or access erasure operations. This
allows a publisher credential to write reviewed shared memory while session
credentials write personal scopes and read the shared scopes. A grant with
`read_scopes` cannot also have `raw_access`.

The field is optional and existing binding files retain their behavior. Servers
older than 0.7.3 refuse a bindings file containing `read_scopes`.

### Bounded cleanup of expired receipts

Successful `remember`, `forget` and `consolidate` writes reclaim a bounded
number of receipt/result groups that expired at least five minutes earlier.
This includes eligible receipts created by earlier releases. Each write
examines at most 512 receipts and removes at most 1,024 graph nodes, except
that one larger receipt group may be removed when it fits in the write.

Cleanup shares the caller's commit. If adding cleanup would make that commit
too large, the operation commits on its own and cleanup waits for a later
write. Expired receipts already answer as missing; this does not change the
retry window. Removal is logical, not secure erasure: old bytes may remain in
the log and backups until those are compacted or expire.

### Restart-free grant reload

For a binding the server already serves, an administrator can edit the private
bindings file, raise the changed binding's `generation`, replace the file
atomically, then call `POST /v1/admin/memory/bindings/reload`. The dashboard
Administration workspace also exposes the reload action. The endpoint applies
only changes to principals, `scopes`, `read_scopes` and `raw_access`; it does
not edit the file.

Requests pin the grants version they first consult. A durable grant update is
served before subsequent requests use it, and a write admitted under superseded
grants fails closed. The response's `drained` value reports whether requests
using older grants have finished. If it is false, resend the request to wait
again; disconnecting the caller does not cancel an apply already in progress.
The [memory guide](MEMORY.md#change-grants-live) documents the full procedure.

This is not a binding lifecycle API. Adding, removing, renaming or retiring a
binding; changing its tenant, graph, evidence collection, state or receipt
window; and enabling memory for the first time still require a restart. Reload
is unavailable when a gRPC router was built; the server continues to refuse a
gRPC listener while memory bindings are configured.

## Upgrade

Back up the database and verify the release checksums first. Stop the old
server before starting 0.7.3; keep one writer per file. There is no format
change, conversion or migration. To use read-only shared scopes, upgrade the
server first, then add `read_scopes` with a higher binding generation. Older
servers refuse that field. Grant reload applies only after memory is already
enabled; initial setup still requires a restart.

REST clients do not need an upgrade unless they call the new admin endpoint
directly. The endpoint and its capability flag are documented in the
[API reference](API.md#change-grants-without-a-restart).

## Rollback

Stop 0.7.3 before starting the previous server. When the bindings file does not
use `read_scopes`, the unchanged file format and binding records require no
migration; grant-only reloads have the same durable records as a restart with
that file.

If the file uses `read_scopes`, older servers refuse to start with it. Before
rolling back, remove `read_scopes` and remove or rewrite read-only grants so
each remaining `scopes` list grants only access that should be read/write on
the older server. Raise the generation of every changed binding, then restart
the older server. Review this downgrade carefully: older grants do not
distinguish read-only from read/write access.

## Compatibility and known limits

- No database format, file migration, or new server setting is introduced.
- Read-only grants affect `recall` and the currently unavailable
  `facts/query` operation only. Memory remains opt-in and REST-only.
- Grant reload applies only grant changes to existing bindings. Binding
  lifecycle changes and first-time setup still need a restart.
- The new receipt cleanup is bounded and best-effort within each successful
  write; a receipt backlog clears only as later writes occur.
- The gRPC listener remains disabled while memory bindings are configured.
- The 0.7.3 release does not add memory facts/erasure APIs, self-service
  binding management, a file watcher, gRPC memory or gRPC live reload.
- No benchmark campaign was run for this minor release; existing benchmark
  results do not measure 0.7.3.
