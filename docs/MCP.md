# VLite MCP Server

Connect your coding assistant directly to a local VLite database with
`vlite-mcp`. Install the executable, choose a database and add it to your
assistant's MCP configuration.

## What you get

VLite provides **one local MCP server**, `vlite-mcp`. It lets an assistant search
and inspect your database without a REST server, dashboard or bearer token.
It is not a connector to an already-running VLite API.

- **Read-only tools:** database information, collection listing, exact/ANN
  vector search, record fetch/pagination, index status and in-memory index builds.
- **Optional write tools:** insert/replace and delete records, only when you
  explicitly launch with `--writable`.
- **Transport:** local stdio; the agent host launches the process. It has no
  HTTP/SSE endpoint or network listener.

The separate [four agent skills](AGENT-SKILLS.md) teach an assistant workflows;
they are not server executables. Installing a skill does not install this server.

## Install and launch

### Option 1: prebuilt binary (no Rust required)

1. Open [GitHub releases](https://github.com/mahpate/Vlite/releases) and download
   the native VLite archive for your OS/architecture, plus `SHA256SUMS`.
2. [Verify the archive](INSTALL.md#verify-before-installing), extract it, and locate
   `vlite-mcp.exe` on Windows or `vlite-mcp` on Linux/macOS.
3. Keep it in a permanent location. Use its absolute path as the client's
   `command`, or add its directory to your `PATH`. On Linux/macOS, run
   `chmod +x /path/to/vlite-mcp` if extraction did not preserve executable permissions.
4. Run `vlite-mcp --help` (Windows: `& 'C:\path\to\vlite-mcp.exe' --help`) to
   check the installation.

Installing `vlite-embedded`/`vlite-client` with pip does **not** install MCP.

### Option 2: build from source

Requires Git and [Rust](https://www.rust-lang.org/tools/install) 1.89 or newer.
From a terminal:

```sh
git clone https://github.com/mahpate/Vlite
cd Vlite
cargo install --locked --path . --features mcp --bin vlite-mcp
vlite-mcp --help
```

Cargo installs into `~/.cargo/bin` (Windows: `%USERPROFILE%\.cargo\bin`).
Restart your terminal after installing Rust, or put that directory on `PATH`.

### Choose a database before connecting

Use an existing database file and an absolute path in client configuration.
The server does not create a missing database. To make a small test database,
use the [embedded quickstart](https://mahpate.github.io/Vlite/#quickstart), or
from the source checkout run `cargo run --locked --example basic -- demo.vlite`
with a new, nonexistent output path.

Stop any exclusive owner of that file first, including `vlite-server`. Multiple
read-only MCP processes can coexist; they cannot share the file with an active
writer. This is a local file connection, not the API URL of a Docker server.
Do not try to bypass Docker volume ownership to open a running server's file.

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

### Check that the connection works

Save the configuration and start/reload the server in your agent host. Approve
that specific local server if the host requests trust. Ask:

> Use VLite to show database information and list the collections. Do not write.

The host should discover the seven read-only tools listed below and call
`vlite_info` / `vlite_list_collections`. Supply a compatible query vector when
asking for a search; this MCP server does not generate embeddings.

If the command is not found, use an absolute executable path (JSON Windows
paths need doubled backslashes, e.g. `C:\\Tools\\VLite\\vlite-mcp.exe`).
If the file is missing, correct `--db`; if it is locked, stop its writer rather
than deleting the file. A normal terminal launch waits for MCP protocol input,
so it will not present a chat prompt. Read errors on stderr in the host's server logs.

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
| `vlite_upsert` | `id`, `vector`, `collection?`, `metadata?`, `payload?` or `payload_base64?` | Writable mode only. Payload is UTF-8 text or canonical standard base64 bytes. |
| `vlite_delete` | `id`, `collection?` | Writable mode only. |

All tool inputs are JSON-schema-described. The server speaks newline-delimited
JSON-RPC 2.0, including the MCP lifecycle, `ping` and tool discovery/calls.
Protocol messages alone are written to stdout; diagnostics are written to stderr.
Tool failures return `isError: true` with a clear message; malformed JSON and protocol errors use JSON-RPC errors. `vlite_search` returns IDs and distances, not vectors. `vlite_get` and `vlite_list` omit vectors unless explicitly requested by `vlite_get`.

<a id="property-graph-tools-experimental"></a>

## Property graph tools

The released `vlite-mcp` includes the graph tools and the `mcp-proxy`
feature. Since v0.5.1 the graph tools are compiled into every `mcp` build. To
build the same binary from source:

```sh
cargo install --locked --path . --features "mcp mcp-proxy" --bin vlite-mcp
```

The graph tools are listed and callable only when the opened file is
graph-enabled (created with graph support). On a vector-only file, `tools/list`,
`vlite_info` and every call are byte-identical to a vector-only server, and
calling a graph tool returns `unknown tool`. On a graph-enabled file
`vlite_info` adds `graph_enabled: true` and `graphs` (name, node and edge counts).

Read tools (always listed on graph-enabled files):

- `vlite_graph_list` — graphs with node/edge counts. No arguments.
- `vlite_graph_get` — `graph` and exactly one of `node` or `edge`, each
  `{"key": ...}` or `{"id": "<u64>"}`. A missing node/edge returns `null`.
- `vlite_graph_find_nodes` — `graph`, `label?`, typed `conditions?`
  (the `vlite_search` filter syntax), `limit?`, `cursor?`.
- `vlite_graph_neighbors` — `graph`, `node`, `direction?` (`out`/`in`/`both`),
  `types?`, `edge_conditions?`, `limit?`, `cursor?`.
- `vlite_graph_traverse` — `graph`, `start` (selectors), `direction?`, `types?`,
  `expand_filter?`, `return_filter?`, `edge_filter?`, `include_paths?`.
- `vlite_graph_shortest_path` — `graph`, `source`, `destination`,
  `direction?`, `types?`. `outcome` is `found`, `no_path_within_hops` (fully
  explored) or `exhausted` (limits hit; absence is unknown).
- `vlite_graph_schema` — `graph`, `budget?` (default 100000 entities). Returns
  the `declared` schema (labels, relationship types, endpoints, typed
  properties, indexes) and an `observed` summary with counts and `origin`
  (`declared`, `observed` or `both`); `observed.complete` is false when the
  budget stopped the scan. Call it before writing or querying an unfamiliar graph.
- `vlite_graph_retrieve` — `graph`, `pivot` (`collection` plus `vector` and/or
  `text`, `k?`, `filter?`, `metric?`), `expansion?` (`direction`, `types`,
  `expand_filter`, `return_filter`, `edge_filter`, `include_paths`),
  `evidence?` (`collections`, `fields`, `max_excerpt_bytes`, `max_items`),
  `limits?`, `projection?`. Pivot records map to attached nodes, the graph is
  expanded within the limits, and records attached to returned nodes come back
  as evidence (metadata and payload excerpts, never vectors). Check
  `complete`, `truncation` and `unmapped_pivots`.
- `vlite_graph_query` — `graph`, `query` (the read-only text of
  [GRAPH-QUERY.md](GRAPH-QUERY.md)), `params?` (typed `$name` values),
  `limits?`, `explain?`. Each text maps to exactly one typed read and returns
  that tool's output; rejected text fails `graph_query_*` before any graph access.

Every query tool also accepts `projection?` (`"all"`, `"none"` or
`{"only": [...]}`), `limits?` and `explain?`. With `explain: true` the tool
returns the normalized query, effective limits and planned indexes without
executing. `limits` fields default to `max_hops` 2, `max_visited_nodes` 1000,
`max_edges_examined` 10000, `max_edges_per_node` 1000, `deadline_ms` 250,
`max_output_nodes` 100, `max_output_edges` 200 and `max_output_paths` 20. Their
ceilings are 4, 10000, 100000, 10000, 5000, 1000, 2000 and 100; larger values
fail with `graph_limit_exceeds_ceiling` rather than being clamped.

Write tools (listed only with `--writable`):

- `vlite_graph_create` / `vlite_graph_drop` — `graph`. Drop fails
  `graph_not_empty` while the graph has nodes.
- `vlite_graph_put_schema` — `graph`, `schema`. Replaces the declared schema in
  one commit; data that does not conform fails `graph_schema_violation` and
  nothing changes.
- `vlite_graph_write` — `graph` and `ops` (at most 4096): `create_node`,
  `upsert_node`, `patch_node`, `delete_node`, `create_edge`, `ensure_edge`,
  `patch_edge`, `delete_edge`, `attach`, `detach`, `put_vector`,
  `delete_vector`, `put_schema`. `ensure_edge` creates an edge only if the
  same (source, type, destination) is absent and needs a relationship declared
  `multiplicity: "unique_triple"`. The batch
  commits atomically in order; on error nothing is written. `expected`
  (`"any"`, `"absent"` or `{"revision": "<u64>"}`) makes an op conditional
  (`precondition_failed`). Reusing an edge `key` for a different edge fails
  `graph_key_conflict`.

Outputs are compact JSON matching `vlite-graph-cli`: u64 IDs,
revisions and cursors are strings, vectors are never returned, and every read
reports `complete`, `truncation`, `stats` and `cursor`. Treat a result with
`complete: false` as partial. Failures return `isError: true` with
`"<code>: <message>"`, for example `invalid_request: ...`,
`read_only: ...`, `graph_not_found: ...` or `graph_limit_exceeds_ceiling: ...`.
A read-only instance sees the graph as of the moment it opened the file; a
writable instance holds the file's exclusive lock like any other writer.

### Server proxy mode (`mcp-proxy` builds, including releases)

```sh
cargo install --locked --path . --features mcp-proxy --bin vlite-mcp
VLITE_TOKEN=... vlite-mcp --server https://vlite.example:8080 --token-env VLITE_TOKEN
```

With `--server URL`, `vlite-mcp` opens no file. It forwards the graph tools to
a `vlite-server` built with `graph`, through the REST graph routes, so several
agents can share one database. Only graph tools are proxied; vector tools are
not listed. `tools/list` probes `GET /v1/graphs` and lists graph tools only
when the server reports a graph-enabled database. The server's credential
decides what each call may do (role, graph scope, and collection read access
for retrieve evidence). Retrieve output adds `redacted` (`count` and
`collections`) for evidence the token could not read. `--writable` is still
needed before write tools are listed or called. The bearer token is read from
the environment variable named by `--token-env`. It is never taken from the
command line, logged, or echoed in errors. `--server` cannot be combined with
`--db` or key options. `https://` URLs use rustls. Redirects are not followed.
Server errors keep their `<code>: <message>` form; an unreachable server returns
`server_unavailable: ...`.

## Access and locking

Read-only is the safe default and opens the file through VLite's shared-lock read-only API. Multiple read-only processes may open the same file concurrently. Writable mode opens with an exclusive lock and persists each mutation synchronously. Do not run `vlite-mcp --writable` while `vlite-server` or another writer has the same database open. In-memory ANN index construction is available in read-only mode, but its derived index is not persisted.

## Security notes

MCP tools grant a client access to the selected database and should be treated as privileged. Keep stdio configurations local and under user control; select the intended database path explicitly, use read-only mode unless mutations are required, and review tool calls in the host application. Writable mode can replace and delete stored records. Encryption protects database contents at rest but does not restrict access for a process holding the key. Avoid putting keys/passphrases in source-controlled configuration or command history. The stdio server does not bind a network port.
