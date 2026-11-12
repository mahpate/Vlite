# VLite 0.7.5 upgrade and rollback

VLite 0.7.5 adds an opt-in server switch, `VLITE_METRICS_WITH_MEMORY=1`, that
keeps server-wide metrics and the dashboard views built on them available
while agent memory is enabled.

The database format and binding-file schema are unchanged. No data migration
is required. No benchmark campaign was run for this patch release; earlier
measurements do not measure 0.7.5.

## What is new

### Metrics with agent memory

In 0.7.4, a configured memory bindings file makes `GET /metrics` and
`GET /v1/admin/metrics/summary` answer `403 forbidden_scope`, because their
global totals include memory-backed data. The dashboard marks the views that
depend on them as unavailable; sign-in still works.

Start `vlite-server` with `VLITE_METRICS_WITH_MEMORY=1` to serve both routes
again. Library embedders set `ServerOptions::metrics_with_memory`. The variable
accepts `0`, `1`, `true` or `false`; any other value stops startup. It is read
once at start, so changing it needs a restart, and it is off by default.

With the switch on:

- Both routes answer as they do without memory. Their totals (database bytes,
  record and collection counts, search counters) include memory data, so anyone
  who can read metrics learns how much memory data exists. Leave the switch off
  when those aggregate sizes must stay private.
- Reserved memory graph and evidence names never appear in per-collection
  rows, and scoped callers still see only their own collections.
- Nothing else about memory changes: reservations, grants and every other
  memory guard behave as before.

See [Memory-bound resources](SECURITY.md#memory-bound-resources) and the
[dashboard guide](CONSOLE.md#monitor-the-server).

## Upgrade

Verify release checksums and back up the database, private bindings file and
admin state. Stop the old server before starting 0.7.5; keep one writer per
database file. Preserve existing durability, authentication, proxy and storage
settings. There is no format conversion or binding-generation change solely
for this upgrade.

Existing clients and the dashboard work unchanged. To use the switch, add
`VLITE_METRICS_WITH_MEMORY=1` to the server's environment for the upgrade
restart, then check that `GET /metrics` answers `200`. Without the variable,
0.7.5 behaves as 0.7.4 does.

When upgrading from 0.7.2 or earlier, also read the
[0.7.3 compatibility guidance](RELEASE-0.7.3.md), especially before introducing
read-only shared scopes.

## Rollback

Stop 0.7.5 before starting 0.7.4. The unchanged database format and binding
schema require no migration. 0.7.4 ignores `VLITE_METRICS_WITH_MEMORY`, so the
two routes answer `403` again while memory is enabled; the variable can stay set
or be removed.

For rollback to 0.7.2 or earlier, follow the
[0.7.3 rollback instructions](RELEASE-0.7.3.md#rollback). Those releases
refuse bindings files containing `read_scopes`; never turn read-only grants
into read/write grants without reviewing the intended access.

## Compatibility and known limits

- Memory remains opt-in and REST-only.
- The switch lifts only the two metrics routes. Graph statistics, replication
  export, webhook list and status, imports and automatic-maintenance routes
  still answer `403 forbidden_scope` while a binding exists, and the server
  still refuses `--grpc-bind` and scheduled automatic index maintenance.
- Totals do not separate memory data from other data, and there is no
  per-scope breakdown of reserved resources.
- This release does not add managed followers, multi-writer operation,
  automatic failover or a larger request-concurrency limit.
- Azure Files remains an experimental deployment option, not power-loss
  or cross-host writer-lock qualification.
