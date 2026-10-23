# VLite MCP Server

`vlite-mcp` exposes a VLite database to MCP clients over the standard input/output (stdio) transport. It speaks newline-delimited JSON-RPC 2.0 and supports the MCP lifecycle, `ping`, and tool discovery/calls. Protocol messages alone are written to stdout; diagnostics are written to stderr.

## Install and launch

Build from the VLite checkout with the small `mcp` feature (which enables JSON support):

```sh
cargo install --path . --features mcp
vlite-mcp --db ./vectors.vlite
```

Release builds that include the `mcp` feature can run the same binary. No HTTP client or REST proxy is required; this server opens the embedded database file directly.

The server is **read-only by default**. Pass `--writable` to expose `vlite_upsert` and `vlite_delete`. `--read-only` is accepted explicitly for configurations where the safe mode should be visible. Encryption options mirror the other VLite binaries: `--encryption-key-file PATH`, `VLITE_ENCRYPTION_KEY`, or `VLITE_ENCRYPTION_KEY_HEX` (choose exactly one). A 32-byte key file is interpreted as raw key bytes; other key-file lengths are treated as passphrases, with trailing CR/LF removed.

## Client configuration

### VS Code (`.vscode/mcp.json`)

```json
{
  "servers": {
    "vlite": {
      "type": "stdio",
      "command": "vlite-mcp",
      "args": ["--db", "${workspaceFolder}/data.vlite", "--read-only"]
    }
  }
}
```

### GitHub Copilot CLI

Add the server to the MCP servers in the CLI's MCP configuration (`~/.copilot/mcp-config.json`):

```json
{
  "mcpServers": {
    "vlite": {
      "command": "vlite-mcp",
      "args": ["--db", "/absolute/path/to/data.vlite", "--read-only"]
    }
  }
}
```

### Claude Desktop (`claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "vlite": {
      "command": "vlite-mcp",
      "args": ["--db", "/absolute/path/to/data.vlite", "--read-only"]
    }
  }
}
```

For an encrypted database, supply the encryption key through the process environment or add `--encryption-key-file` to `args`. Do not place passphrases directly in shared configuration files.

## Tools

| Tool | Arguments | Behavior |
| --- | --- | --- |
| `vlite_info` | none | Format version, aggregate count, collection dimensions/counts, file size, and read-only state. |
| `vlite_list_collections` | none | Collection names, dimensions, IDs, and live counts. |
| `vlite_search` | `vector` (required), `collection?`, `k` (1–100, default 10), `filter?`, `metric?`, `mode?` | Exact by default; `mode: "approximate"` uses the available HNSW path. Filters use VLite's typed conditions, e.g. `[{"op":"eq","field":"tag","value":{"type":"string","value":"news"}}]`. Metrics: `squared_l2`, `cosine`, `inner_product`. |
| `vlite_get` | `id` (u64 number/string), `collection?`, `include_vector?` | Fetch a record; vector omitted unless requested. |
| `vlite_list` | `collection?`, `after?` (u64), `limit?` (1–100, default 20) | Page records in ascending ID order; vectors omitted. |
| `vlite_index_status` | `collection?` | In-memory HNSW readiness and maintenance pressure. |
| `vlite_build_index` | `collection?` | Build an in-memory HNSW index; permitted in read-only mode and does not change the file. |
| `vlite_upsert` | `id`, `vector`, `metadata?`, `payload?` or `payload_base64?` | Writable mode only. Payload is UTF-8 text or canonical standard base64 bytes. |
| `vlite_delete` | `id`, `collection?` | Writable mode only. |

All tool inputs are JSON-schema-described. Tool failures return `isError: true` with a clear message; malformed JSON and protocol errors use JSON-RPC errors. `vlite_search` returns IDs and distances, not vectors. `vlite_get` and `vlite_list` omit vectors unless explicitly requested by `vlite_get`.

## Access and locking

Read-only is the safe default and opens the file through VLite's shared-lock read-only API. Multiple read-only processes may open the same file concurrently. Writable mode opens with an exclusive lock and persists each mutation synchronously. Do not run `vlite-mcp --writable` while `vlite-server` or another writer has the same database open. In-memory ANN index construction is available in read-only mode, but its derived index is not persisted.

## Security notes

MCP tools grant a client access to the selected database and should be treated as privileged. Keep stdio configurations local and under user control; select the intended database path explicitly, use read-only mode unless mutations are required, and review tool calls in the host application. Writable mode can replace and delete stored records. Encryption protects database contents at rest but does not restrict access for a process holding the key. Avoid putting keys/passphrases in source-controlled configuration or command history. The stdio server does not bind a network port.

