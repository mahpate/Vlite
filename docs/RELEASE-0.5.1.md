# VLite 0.5.1 upgrade and rollback

This release changes the default durability, adds an in-memory-only mode, and
compiles the property graph into every build. It follows
[0.5.0](RELEASE-0.5.0.md). VLite remains experimental: the live dataset resides
in memory, startup replays the log, and one exclusive writer or several
read-only owners may open a file. File formats are unchanged.

## Behavior change: periodic durability is the default

File handles and `vlite-server` now default to periodic (lazy) durability with
a 60 s interval (`Durability::default()`, `DEFAULT_FLUSH_INTERVAL`). Commits are
written to the OS before they are acknowledged and synced at most once per
interval, on clean close and before change-feed reads or log exports.

- A process crash loses nothing that was acknowledged.
- An OS crash or power loss can lose up to ~60 s of acknowledged writes.
- To restore the 0.5.0 behavior (sync before every acknowledgment), use
  `Durability::Full`, `vlite-server --durability full`, or `durability: "full"`
  in the C ABI and Python/Node bindings.
- `--flush-interval-ms` (1–60000) no longer requires `--durability periodic`.

See [periodic durability](REFERENCE.md#periodic-lazy-durability).

## New: durability `none` (in memory only)

`Durability::None` keeps the database entirely in memory; nothing is written to
disk and all data is lost when the process or service restarts. It is the mode
of in-memory handles (`Database::create_in_memory`, `open_from_bytes`); selecting
it on a file handle is rejected, so a file-backed handle never silently drops
writes.

`vlite-server --durability none --create-if-missing <dimensions>` serves a
fresh in-memory database and never creates, reads, locks or writes the
database path. It prints a startup warning and rejects `--flush-interval-ms`,
`--ann-snapshot`, encryption and `VLITE_ADMIN_DIR` (backups). The C ABI and
Python/Node bindings accept `durability: "none"` only for in-memory handles.

## Property graphs are no longer experimental

Graph support is compiled into every build; the `graph` Cargo feature remains
as a no-op alias so existing `--features graph` commands still build. Files
stay plain unless graph is requested (`create_with_graph`,
`vlite-graph-cli init`, `vlite-maintain enable-graph`). Graph-enabled files
still do not support change feeds, webhooks or gRPC, and
`vlite-maintain strip-graph --confirm-graph-data-loss` still rolls them back.
See [GRAPH](GRAPH.md) and [TOOLS](TOOLS.md#graph-operations).

## Other changes

- Capabilities no longer advertise `change_feed` or `webhooks` for
  graph-enabled files.
- Release images are built natively per architecture and ship only the needed
  binaries, which shortens image builds.

## Performance

The graph-off performance baseline refresh (DO-005) remains deferred.
Published comparisons still use the historical baseline `81ff901`; this
release makes no new benchmark claims.

## Upgrade and rollback

Back up and verify checksums before upgrading. Existing files require no
migration. Deployments that relied on sync-per-commit must pass
`--durability full` (or `Durability::Full`) after upgrading. Rolling back to
0.5.0 restores full durability by default; graph-enabled files open in 0.5.0
only with builds that include `graph`. GitHub assets, container verification
and PyPI publication are separate stages; verify each destination before
installing.
