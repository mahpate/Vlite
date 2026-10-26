# VLite 0.5.0 upgrade and rollback

This release ships the opt-in property graph in the distributed artifacts and
packages the dashboard, account, durability and indexing work since
[0.4.1](RELEASE-0.4.1.md). VLite remains experimental: the live dataset resides
in memory, startup replays the log, and one exclusive writer or several
read-only owners may open a file. Vector-only files keep their format.

## Experimental property graphs (opt-in)

VLite can store named property graphs alongside vectors in one file, with
atomic mixed graph/vector writes, bounded traversal and shortest paths, schemas
with property indexes, hybrid vector/text-to-graph retrieval and a read-only
query language. Graph support is off unless enabled; graph-enabled files carry
a header flag and are refused by builds without graph support. Roll back with
`vlite-maintain strip-graph --confirm-graph-data-loss`. Change feeds and gRPC do
not support graph files.

- Release binaries, the C library, wheels and Node release tarballs are built
  with the `graph` feature; `vlite-mcp` also with `mcp-proxy`.
  The Docker image's API server (`vlite-server`) has graph; `vlite-graph-cli`
  is not in the image. The archives add `vlite-mcp` and `vlite-graph-cli`. The Cargo `default` feature
  set and the default `vlite-ffi` crate build stay graph-free.
- A file holds graphs only after `create_with_graph`, `vlite-graph-cli init`
  or `vlite-maintain enable-graph`; other files behave as before.
- Surfaces: the embedded Rust API, `vlite-graph-cli`, MCP `vlite_graph_*`
  tools (on a file or through `vlite-mcp --server`), REST `/v1/graphs` routes
  including schema, retrieve and query ([OpenAPI fragment](../src/openapi_graph.json)),
  C/Python/Node graph commands, and a Dashboard Graph workspace.
- Graphs are tenant-owned. `vlk_` keys need `graph_read`/`graph_write` and a
  `graphs` binding; signed-in accounts get role-based access narrowed by the
  new account fields `graphs` and `graph_access`.

See [GRAPH](GRAPH.md), [GRAPH-QUERY](GRAPH-QUERY.md), [MCP](MCP.md) and
[TOOLS](TOOLS.md#graph-operations). Names and outputs may change.

## Other changes

- Dashboard: sign-in-first landing, grouped workspaces and a refreshed style;
  user accounts with Argon2id passwords and cookie sessions, password change,
  user management, a signed-in identity badge and tenant lifecycle
  administration (usage, limits, suspend/resume, delete).
- Opt-in periodic (lazy) durability: `Durability::Periodic` syncs at most once
  per interval. `Durability::Full` remains the default.
- HNSW: parallel, thread-count-independent construction and vectorized
  distances; opt-in ANN snapshots in the C ABI and Python.
- Faster Python float32 ingest and typed `approximate_ready` search.
- Change-feed, point-in-time restore and disk-index request errors answer 4xx
  instead of 500.
- An experimental Azure Container Apps deployment in `deploy/azure-container-apps`.

## Performance

The owner deferred the graph-off performance baseline refresh (DO-005) until
after this release. Published comparisons still use the historical baseline
`81ff901`; this release makes no new benchmark claims.

## Upgrade and rollback

Back up and verify checksums before upgrading. Existing V3/V4/V5 files require
no migration. Graph-enabled files cannot be opened by 0.4.x or by builds
without `graph`; strip them first. Older servers have no password sign-in, so
keep an admin token configured before rolling back. Derived ANN indexes and
snapshots can be rebuilt. GitHub assets,
container verification and PyPI publication are separate stages; verify each
destination before installing.
