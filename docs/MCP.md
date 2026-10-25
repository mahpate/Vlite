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

## Access and locking

Read-only is the safe default and opens the file through VLite's shared-lock read-only API. Multiple read-only processes may open the same file concurrently. Writable mode opens with an exclusive lock and persists each mutation synchronously. Do not run `vlite-mcp --writable` while `vlite-server` or another writer has the same database open. In-memory ANN index construction is available in read-only mode, but its derived index is not persisted.

## Security notes

MCP tools grant a client access to the selected database and should be treated as privileged. Keep stdio configurations local and under user control; select the intended database path explicitly, use read-only mode unless mutations are required, and review tool calls in the host application. Writable mode can replace and delete stored records. Encryption protects database contents at rest but does not restrict access for a process holding the key. Avoid putting keys/passphrases in source-controlled configuration or command history. The stdio server does not bind a network port.
