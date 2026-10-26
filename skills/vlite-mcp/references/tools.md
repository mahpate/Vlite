# Expected MCP tool surface

The stdio binary is `vlite-mcp`; core flags are `--db <file>` and optional
`--writable`. Default is read-only. This surface may evolve: discovery schemas,
installed `--help` and repository `docs/MCP.md` are authoritative. Argument
schemas are deliberately not duplicated; they need not match REST/embedded APIs.

- `vlite_info`: database identity, format, dimensions and mode; call first.
- `vlite_list_collections`: discover collections before addressing records.
- `vlite_search`: vector retrieval with bounded k and supported typed filters.
- `vlite_search_text`: text retrieval **only if an embedder is configured** and
  the tool is advertised. Match the stored model/dimensions.
- `vlite_get`: fetch selected record evidence by ID in its collection.
- `vlite_list`: bounded/paginated browsing, not a full-database dump.
- `vlite_upsert`: insert/replace a complete record, **writable mode only**.
- `vlite_delete`: remove a record, **writable mode only**.
- `vlite_index_status`: inspect readiness before approximate retrieval/build.
- `vlite_build_index`: explicitly build when available/authorized; it may be
  expensive even when changing only in-memory state.

### Experimental graph tools (source-only `graph` builds)

Advertised only by builds with the `graph` feature, and only for graph-enabled
files. See `docs/MCP.md` for arguments, limits and error codes.

- `vlite_graph_list`: graphs and node/edge counts; call before graph queries.
- `vlite_graph_get`: one node or edge by key/ID; `null` means absent.
- `vlite_graph_find_nodes`: label/property lookup with cursor pagination.
- `vlite_graph_neighbors`: one node's edges and adjacent nodes.
- `vlite_graph_traverse`: bounded multi-hop expansion; check `complete`.
- `vlite_graph_shortest_path`: `found`, `no_path_within_hops` or `exhausted`.
- `vlite_graph_schema`: declared schema plus an observed, budgeted summary.
- `vlite_graph_retrieve`: vector/text pivot, bounded expansion and compact
  evidence records; check `complete` and `unmapped_pivots`.
- `vlite_graph_query`: one read-only `MATCH ... RETURN` text mapped to a typed read.
- `vlite_graph_create`, `vlite_graph_drop`, `vlite_graph_put_schema`,
  `vlite_graph_write`: **writable mode only**; `vlite_graph_write` applies an
  atomic batch of operations (including `ensure_edge` and `put_schema`).

`mcp-proxy` builds also accept `--server URL --token-env VAR`: graph tools
are forwarded to a `vlite-server`, which authorizes each call; vector tools
are not proxied.

Every graph query accepts `limits` (rejected above their ceilings), `projection`
and `explain` (retrieve has no `explain`).

## Host launch shape

For hosts accepting `mcpServers`, the launch shape is typically:

```json
{
  "mcpServers": {
    "vlite": {
      "command": "vlite-mcp",
      "args": ["--db", "C:\\data\\knowledge.vlite"]
    }
  }
}
```

This is a launch illustration, not a universal configuration filename/schema.
Some hosts use another wrapper (for example `servers`) or require a stdio type.
Follow host MCP documentation and `docs/MCP.md`. Use an absolute binary path if
PATH is not inherited, and an existing database. Do not add `--writable` to
retrieval-only configuration. Keep stdout reserved for protocol messages.

## Failure handling

- Missing tool: rediscover; check version/mode, embedder configuration and,
  for graph tools, a `graph` source build and a graph-enabled file.
- Locked: close legitimate owners or use an authorized closed backup. Do not
  bypass locks or kill unrelated processes.
- Dimensions/model mismatch: use the correct embedding model/collection, never
  pad/truncate arbitrary vectors just to satisfy validation.
- No matches: preserve scope; seek better evidence rather than silently widening.
- Index not ready: select exact if allowed, or explicitly schedule/build a
  matching index. Avoid unexpected expensive first-query builds.
- Incomplete/corrupt file: stop and follow recovery guidance; never automatically
  truncate, repair or recreate the database.
