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

- Missing tool: rediscover; check version/mode and embedder configuration.
- Locked: close legitimate owners or use an authorized closed backup. Do not
  bypass locks or kill unrelated processes.
- Dimensions/model mismatch: use the correct embedding model/collection, never
  pad/truncate arbitrary vectors just to satisfy validation.
- No matches: preserve scope; seek better evidence rather than silently widening.
- Index not ready: select exact if allowed, or explicitly schedule/build a
  matching index. Avoid unexpected expensive first-query builds.
- Incomplete/corrupt file: stop and follow recovery guidance; never automatically
  truncate, repair or recreate the database.
