# Property graphs (experimental, opt-in)

VLite can store named property graphs (nodes, typed edges and attachments to
vector records) in the same file and commit log as vectors. Graph support is
the non-default `graph` Cargo feature. Since v0.5.0 the release binaries,
C library, wheels and Node release tarballs are built with it (`vlite-mcp` also
with `mcp-proxy`). The Docker image's API server (`vlite-server`) has graph;
`vlite-graph-cli` is not in the image. It stays opt-in per file: graphs exist
only in files created with `create_with_graph` or `vlite-graph-cli init`, or copied with
`vlite-maintain enable-graph`; other files behave as vector-only files.

## Get it

| Surface | Release artifact | Source checkout |
|---------|------------------|-----------------|
| Rust | — | `vlite` with `features = ["graph"]` |
| CLI | `vlite-graph-cli` in the release archive | `cargo install --locked --path . --features graph,json --bin vlite-graph-cli` |
| MCP | `vlite-mcp` in the release archive | `cargo install --locked --path . --features "mcp graph mcp-proxy" --bin vlite-mcp` |
| REST | `vlite-server` in the archive and Docker image (`vlite-graph-cli` is not in the image) | `cargo build --locked --release --features server,graph --bin vlite-server` |
| C, Python, Node | release C library, wheels, Node release tarballs | `cargo build --locked --release -p vlite-ffi --features graph` |

Builds without the feature (the Cargo default, the default `vlite-ffi` crate)
have no graph code, routes, tools or OpenAPI entries.

## Compatibility and rollback

- Graph-enabled files are v5 files with header flag `FLAG_GRAPH = 4`; graph
  commits are op-12 frames. Files are never converted in place.
- Builds without `graph` and older releases refuse them with
  `UnsupportedFeature(4)`. Vector-only files behave the same in both builds.
- `vlite-maintain enable-graph SOURCE DESTINATION` copies a v4/v5 file out as
  a graph-enabled file.
- To roll back, run `vlite-maintain strip-graph SOURCE DESTINATION
  --confirm-graph-data-loss` in a graph build. It copies the vectors to a plain
  v5 file and drops the graphs; keep the source if you may need them.
- Backup, restore, verify, compaction, re-encryption and replicas carry graph
  state; `upgrade` rejects graph files. Format rules are in
  [STORAGE-FEATURES](STORAGE-FEATURES.md#graph-enabled-files-experimental-graph-feature).

## Data model and consistency

- Up to 64 named graphs per file; only empty graphs can be dropped.
- Nodes have a unique key, labels and typed properties. Edges have a
  relationship type, endpoints, an optional key and properties. IDs, revisions
  and sequences are u64 (decimal strings in JSON).
- An attachment links a node to a live vector record without expiry. Attached
  records cannot be deleted or given an expiry until detached.
- A write batch of up to 4,096 node, edge, attachment and vector operations
  commits atomically or not at all. `expected` (`any`, `absent`,
  `{"revision": N}`) makes an operation conditional.
- Each read sees one committed state, reported as `view_sequence`. A graph
  write between pages fails the next page with `graph_cursor_stale`.
- One exclusive writer or several read-only handles per file. Graph writes
  follow the database durability mode.

## Limits

Omitted limits use defaults; larger values fail with
`graph_limit_exceeds_ceiling` rather than being clamped.

| Limit | Default | Ceiling |
|-------|---------|---------|
| `max_hops` | 2 | 4 |
| `max_visited_nodes` | 1,000 | 10,000 |
| `max_edges_examined` | 10,000 | 100,000 |
| `max_edges_per_node` | 1,000 | 10,000 |
| `deadline_ms` | 250 | 5,000 |
| `max_output_nodes` / `edges` / `paths` | 100 / 200 / 20 | 1,000 / 2,000 / 100 |

Results report `complete`, truncation reasons and work statistics. A truncated
shortest-path search reports `exhausted`, never an uncertified "no path".

## Schema, retrieval and query text

- **Schema.** `put_schema` declares label and relationship rules (required and
  unique properties, endpoints, multiplicity); violating writes fail with
  `graph_schema_violation`. Declared property indexes serve `find_nodes` with
  scan-identical results and are rebuilt on open.
- **Hybrid retrieval.** `GraphRef::retrieve` runs one vector, text or fused
  pivot search, maps hits to nodes through attachments, expands and returns
  evidence records in one view. `k` defaults to 20 (ceiling 100); exact-scan
  pivots over more than `max_exact_scan_records` (default 100,000) fail with
  `graph_pivot_too_expensive`.
- **Query text.** A small read-only language that maps to one typed read; see
  [GRAPH-QUERY.md](GRAPH-QUERY.md).

## Surfaces

| Surface | What it offers |
|---------|----------------|
| Rust | `create_with_graph`, `create_graph`/`drop_graph`/`list_graphs`, `graph_mut(name)`, `graph(name)`, `explain`, `query_text`, `retrieve` |
| CLI | `vlite-graph-cli`; see [graph operations](TOOLS.md#graph-operations-experimental) |
| MCP | `vlite_graph_*` tools, on a file or through a server with `--server URL` (`mcp-proxy` feature); see [MCP](MCP.md#property-graph-tools-experimental) |
| REST | `/v1/graphs` routes: list, create, drop, gets, find, neighbors, traverse, shortest path, explain, schema, retrieve, query, batch |
| C ABI | `{"op":"capabilities"}` and `{"op":"graph","request":{...}}` |
| Python | `supports_graph()`, `create_with_graph()`, `graph_*` methods |
| Node | `supportsGraph()`, `createWithGraph()`, `graph*` methods ([guide](../bindings/node/README.md)) |

**REST access.** Graph routes accept the admin token, the read-only token
(reads only) or `vlk_` credentials with `graph_read` / `graph_write` and a
`graphs` binding naming the graph (or `["*"]`). Signed-in users also have graph
access: `super_admin` and `editor` can read and write, `viewer` can read,
narrowed per account by the `graphs` and `graph_access` knobs below.
Collection scope never implies graph scope. Batches that write vectors also
need write scope on each collection. Retrieve needs `read` on the pivot
collection; evidence from collections the caller cannot read is left out and
counted in `redacted`. Collections are named as stored, so tenant users reach
only their tenant's `t<tenant_id>__<name>` collections.

**Tenant and user scoping (v1.2.20).** Graphs are tenant-owned like
collections. For a tenanted principal (a tenant `vlk_` key or a signed-in
user with a `tenant_id`), graph names in paths, create bodies and `graphs`
bindings are tenant-local: `g` is stored as `t<tenant_id>__g`, responses show
`g`, and lists show only that tenant's graphs. A tenant-local name is at most
28 bytes. The admin token, the read-only token and untenanted principals use
stored names and see every graph. A tenant cannot be deleted while it owns
graphs (409 `tenant_not_empty`). Graph files are unchanged; scoping is by
name only, and the CLI and MCP-on-file paths have no tenants.

| Account field | Values | Effect |
| --- | --- | --- |
| `graphs` | absent/`null` (role default `["*"]`), `[]`, or 1-64 names | Graphs the account may use, within its tenant |
| `graph_access` | absent/`null` (role default), `none`, `read`, `write` | Caps graph operations; never above the role (viewer: `read`) |

`POST /v1/admin/users` and `PUT /v1/admin/users/{id}` accept both (`null`
resets on update); super admins take neither, and a viewer with `write` is
422. Changing either ends the account's sessions. Account views and
`/v1/auth/session` report the stored knobs; `GET /v1/capabilities` `scope`
reports the effective grant. Builds without `graph` refuse both fields.

**Embedded.** Python and Node enable graph methods only when the loaded native
library reports `{"graph": true}`; otherwise every graph method fails with
`unsupported`. Schema, retrieval and query text are not in the C ABI, Python or
Node yet.

## Not supported

- Change feeds, `/v1/changes`, gRPC `Changes`/`WatchChanges` and webhooks
  refuse graph files (`graph_change_feed_unsupported`).
- gRPC has no graph service.
- `vlite-graph-cli` does not open encrypted files (the library does).

## Release note text for GATE-008 (approved for v0.5.0)

> **Experimental property graphs (opt-in).** VLite can store named property
> graphs alongside vectors in one file, with atomic mixed graph/vector writes,
> bounded traversal and shortest paths, schemas with property indexes, hybrid
> vector/text-to-graph retrieval and a read-only query language. Graph support
> is off unless enabled; graph-enabled files carry a header flag and are refused
> by builds without graph support. Roll back with `vlite-maintain strip-graph
> --confirm-graph-data-loss`. Change feeds and gRPC do not support graph files.

The owner approved this text and graph artifacts on 2026-10-25; it ships in
[v0.5.0](RELEASE-0.5.0.md). The graph-off performance baseline refresh
(DO-005) is deferred until after the release.
