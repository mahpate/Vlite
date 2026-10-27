# VLite 0.6.0 upgrade and rollback

This release gives graph-enabled files the same feature set as vector-only
files and makes every new file graph-enabled. It follows
[0.5.1](RELEASE-0.5.1.md). VLite remains experimental: the live dataset resides
in memory, startup replays the log, and one exclusive writer or several
read-only owners may open a file. The file format is unchanged; existing v3,
v4 and v5 files open as before and are never converted in place.

## Graph and vector parity

- **Change feed.** Graph commits appear in the change feed on every surface
  (Rust, REST `GET /v1/changes`, gRPC `Changes`/`WatchChanges`, the C ABI and
  the Python, Node and wasm bindings) as `graph_created`, `graph_dropped` and
  `graph_batch` events. Cursors are unchanged and a commit is never split.
- **Webhooks.** Subscriptions deliver graph events; the optional `graphs`
  filter limits them to named graphs.
- **Format upgrade.** `vlite-maintain upgrade` accepts graph-enabled files
  (a validated copy, since they already use the latest format).
- **Capabilities.** `GET /v1/capabilities` reports `change_feed` and
  `webhooks` as true on graph-enabled files.

See [GRAPH](GRAPH.md#change-feed-and-webhooks).

## New files are graph-enabled by default

Every creation path makes a format 5 file with the graph flag: the Rust API
(`Database::create`, `create_in_memory`), `vlite-server --create-if-missing`,
`vlite-shell`/`vlite-import --create`, `vlite-graph-cli init`, the C ABI and the Python, Node and wasm bindings. An explicit format 3 or 4 (for
example `vlite-server --create-format 4` or a binding `format` option) still
creates a plain file. Opening an existing plain file never enables graph; copy
it with `vlite-maintain enable-graph`.

## The `graph` Cargo feature is removed

The property graph is always built. The no-op `graph` feature alias kept by
0.5.1 is gone from `vlite` and `vlite-ffi`.

## Breaking changes

- Rust: exhaustive matches on `ChangeKind` and `GraphChange` must handle the
  graph variants (`GraphCreated`, `GraphDropped`, `GraphBatch`).
- The error codes `graph_change_feed_unsupported` and
  `graph_unsupported_operation` are removed.
- `cargo build --features graph` fails; drop the feature.
- Webhook subscriptions without `events` now receive every event, including
  graph events. List `events` to keep a vector-only subscription.
- Removed graph opt-in APIs: Rust `Database::create_with_graph` and
  `Error::GraphsRequireV5`, Python `create_with_graph`, Node `createWithGraph`,
  the C ABI graph `create` command and the wasm `formatV5` argument. Use the
  default create calls.
- New files default to format 5 with graph. Request format 3 or 4 explicitly
  for a plain file.
- `vlite-shell --create` and `vlite-import --create` make graph-enabled v5
  files.

## Durability defaults are unchanged

File handles and `vlite-server` keep the 0.5.1 default, periodic durability
with a 60 s interval: a process crash loses nothing acknowledged, an OS crash
or power loss can lose up to ~60 s of acknowledged writes. Use
`Durability::Full` or `--durability full` to sync every commit. See
[periodic durability](REFERENCE.md#periodic-lazy-durability).

## Performance

The graph-off performance baseline refresh (DO-005) remains deferred.
Published comparisons still use the historical baseline `81ff901`; this
release makes no new benchmark claims.

## Upgrade and rollback

Back up and verify checksums before upgrading. Existing files require no
migration. Update code that calls the removed APIs, matches change kinds
exhaustively or relies on webhooks without `events` being vector-only.
Graph-enabled files, including every file 0.6.0 creates by default, open in
0.5.1 and in 0.5.0 builds with graph, where the change feed, webhooks and
upgrade reject them; older builds refuse them. To produce a file for an older release, create it with format 4,
or copy it with `vlite-maintain strip-graph SOURCE DESTINATION
--confirm-graph-data-loss`, which drops the graphs. GitHub assets, container
verification and PyPI publication are separate stages; verify each destination
before installing.
