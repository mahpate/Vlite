# Offline command-line tools

Work directly with database files, without a REST or gRPC server.

## Choose a tool

- [Shell](#interactive-or-scripted-shell): inspect records, run searches and perform maintenance.
- [Import](#validated-atomic-or-resumable-imports): validate and load vectors from JSONL, CSV, Parquet, Arrow or FAISS.
- [Search quality](#search-quality-and-configurationdatabase-comparison): measure recall and compare databases or index configurations.
- [Index tuning](#index-tuning-advisor): find HNSW parameters for a target recall.
- [MCP and agent skills](#mcp-server-and-agent-skills): connect an agent to a database through a separate stdio server.
- [Graph operations](#graph-operations): typed reads and atomic writes on graph-enabled files.

**Stop the server first.** The shell, import and tuning tools take an exclusive
file lock, even with the shell's `--read-only` guard. MCP opens a shared-lock
read-only snapshot by default, so it can run beside other readers but not
beside a writer.

## Build and install

Requires Rust 1.89 or later:

```powershell
cargo build --locked --features tools --bins
# Optional Parquet and Arrow IPC readers:
cargo build --locked --features import-formats --bins
```

`tools` enables JSON and CSV; `import-formats` adds Arrow and Parquet. Neither
is in the default embedded build. Binaries are written to `target/debug/`.
Each tool supports `--help` and exits nonzero on errors. For packaged binaries,
see [installation](INSTALL.md).

## Interactive or scripted shell

```powershell
vlite-shell --db example.vlite --create 3
vlite-shell --db example.vlite --read-only --script commands.txt
Get-Content commands.txt | vlite-shell --db example.vlite --read-only
```

`--create N` creates a new graph-enabled v5 database and refuses an existing path.
Without it, the database must exist. Enter one command per line:

```text
help
header
format
collections
stats
use default
put {"id":1,"vector":[1,0,0],"payload":"first","metadata":{"tag":{"type":"string","value":"blue"}}}
get 1
browse 20
search {"vector":[1,0,0],"k":5,"metric":"squared_l2","filters":[{"op":"eq","field":"tag","value":{"type":"string","value":"blue"}}]}
search {"vector":[1,0,0],"k":5,"ann":true,"ef_search":100}
rebuild
delete 1
verify
backup backup.vlite
compact compacted.vlite
upgrade upgraded.vlite
quit
```

- Search accepts typed filters and the exact metrics `squared_l2`, `cosine`
  and `inner_product`. ANN supports L2 and cosine.
- JSON arguments run to the end of the line, so strings need no shell quoting.
  Lines starting with `#` are comments.
- Scripts and stdin stop at the first error; earlier writes stay committed.
  The interactive shell prints errors and continues.
- `browse LIMIT [AFTER_ID]` returns 1–1,000 rows in ascending ID order and
  prints `next_after` for the next page. Payloads display as lossy UTF-8.
- `--read-only` blocks put, delete, create and copy-out maintenance; ANN and
  rebuild change only in-memory indexes. For a shared-lock reader, use
  `Database::open_read_only` or `vlite-mcp`.

## Validated, atomic or resumable imports

```powershell
vlite-import --db new.vlite --create 3 --input rows.jsonl --format jsonl
vlite-import --db new.vlite --input embeddings.csv --format pgvector
vlite-import --db new.vlite --input vectors.parquet --format parquet
vlite-import --db new.vlite --input flat.faiss --format faiss
# Chunked and resumable; rerun with the same input and options:
vlite-import --db new.vlite --input large.jsonl --format jsonl --chunk-size 1000 --job 42
```

By default, `vlite-import` reads and validates the whole input, then commits it
as one atomic import. Any parse, dimension, non-finite vector, payload,
metadata or frame-size error publishes no rows. Errors report row numbers (the
first 20 syntax errors) or stable engine error codes.

- Imports need format v4. Upgrade older files with `vlite-maintain upgrade`.
  `--collection NAME` selects an existing named collection.
- One atomic import holds at most 4,095 rows (one slot is reserved for the
  checkpoint) and must fit the engine's 16 MiB frame budget. The byte precheck
  is conservative and may reject inputs just below the limit.
- Larger inputs need `--chunk-size 1..4095`. All rows are still validated
  first, but **I/O failures or checkpoint conflicts can leave earlier chunks
  committed**. There is no transaction across chunks.
- `--job ID` makes chunked imports resumable. A retry resubmits the last
  committed chunk with its engine digest, skips earlier chunks, and can replay
  a finished job without writes. Keep the input, collection, chunk size and job
  ID unchanged. Earlier skipped chunks cannot be revalidated, and aborted jobs
  cannot resume.
- Empty inputs import zero rows without creating a job. Success prints the job
  ID, row count (including earlier committed rows on retry) and chunks as JSON.
- The whole input is held in memory; this is not a streaming loader.
- Repeated IDs follow ordered upsert semantics: later rows replace earlier
  ones. `--create` never overwrites an existing path.

### Supported input schemas

- **JSONL:** one object per line with `id` (u64 JSON number), `vector` (float
  array), and optional `payload` (UTF-8 string) and `metadata` (typed object).
  Emit large IDs as exact JSON integers. Unknown fields fail validation.
- **CSV / pgvector CSV:** header `id,vector` plus optional `payload` and
  `metadata` columns, in any order. The vector is a quoted literal such as
  `"[1,2,3]"`; metadata is quoted typed JSON.
- **PostgreSQL COPY text (`--format copy`):** tab-separated `id`, vector
  literal and optional payload. An optional `COPY ... FROM stdin;` header and
  `\.` terminator are recognized. Payloads support `\\`, `\t`, `\n`, `\r` and
  `\N` (empty); other escapes, layouts and SQL dumps are not supported.
- **Parquet** (`import-formats`): columns `id` (uint64 or nonnegative int64),
  `vector` (list, large list or fixed-size list of float32/float64, or a UTF-8
  JSON array), and optional UTF-8 `payload` and `metadata`. Null IDs, vectors
  or elements are rejected; null optional columns are empty. Float64 values
  convert to f32 and overflow is rejected. Uncompressed and Snappy only.
- **Arrow IPC** (`import-formats`): file or stream framing with the Parquet
  schema. Compressed IPC is not supported.
- **FAISS flat:** native little-endian, 64-bit `write_index` output for `IxF2`
  (IndexFlatL2) or `IxFI` (IndexFlatIP). Length, dimension and finiteness are
  checked; trailing data is rejected. IDs are assigned from zero with empty
  payload and metadata. Only vectors are imported, so choose L2 or inner
  product at search time. IVF, PQ, HNSW, IDMap, GPU, transforms, endian
  conversion and 32-bit layouts are not supported. See
  [FAISS's serializer](https://github.com/facebookresearch/faiss/blob/main/faiss/impl/index_write.cpp).

## Search quality and configuration/database comparison

```powershell
vlite-quality --db example.vlite --k 10 --samples 8 --json
vlite-quality --db example.vlite --m 8 --other-m 32 --other-ef-search 200 --json
vlite-quality --db before.vlite --other-db after.vlite --json
```

`vlite-quality` measures ANN recall and latency on a real database, sweeps
parameters, analyzes rank overlap and compares two configurations or two
files. It also reports the engine's own quality result. Comparisons run the
same queries from the primary file, so IDs must mean the same thing in both
files; different dimensions fail. Each side is measured against its own exact
baseline. Files open one after another, so two configurations of the same path
do not conflict.

- Recall@k is intersection ÷ exact count. Overlap is intersection ÷ union
  (Jaccard).
- Rank correlation is the Pearson correlation of rank positions on shared IDs.
  Fewer than two shared IDs give `null`; the aggregate averages defined values.
- Reports use L2. `--json` prints compact JSON; otherwise output is labeled
  text.

## Index tuning advisor

```powershell
vlite-tune --db example.vlite --m 8,16,32 --ef-construction 64,128 --ef-search 20,50,100 --k 10 --samples 8 --target-recall 0.95 --json
```

`vlite-tune` sweeps up to 256 parameter combinations. It builds deterministic
L2 HNSW indexes in memory for the full collection and queries each with the same
seeded sample of stored vectors (samples 1–32, k 1–100). It recommends the
cheapest configuration that meets the target recall@k, using the cost
`mean_query_us + memory_weight * component_MiB` (`--memory-weight`, default 1).
If none qualifies, the recommendation is `null` and the exit status is nonzero.

Reports include build time, mean and p95 latency, recall, Jaccard overlap, rank
correlation and estimated component bytes (vectors and graph links only, not
peak RSS). Stored-vector queries find themselves, which can overstate recall,
so validate with production queries. The file is not changed.

## MCP server and agent skills

These have their own guides:

- **[MCP server](MCP.md):** one local stdio server with seven read-only tools
  and two opt-in write tools. Download a native binary or build `vlite-mcp`.
- **[Agent skills](AGENT-SKILLS.md):** install `vlite-rag`, `vlite-mcp`,
  `vlite-ops` and `vlite-index-tuning` as complete folders in your assistant's
  skill directory.

MCP opens the database file directly, not through REST. Installing a skill
does not start the server or grant write access.

<a id="graph-operations-experimental"></a>

## Graph operations

`vlite-graph-cli` runs one typed property-graph operation per invocation on a
[graph-enabled file](STORAGE-FEATURES.md#graph-enabled-files-opt-in-per-file).
It needs the `json` feature (graph is compiled into every build since v0.5.1).
Release archives ship it since v0.5.0. The Docker image's API server (`vlite-server`) has graph;
`vlite-graph-cli` is not in the image, and packages do not include the CLI.
To build it from a checkout:

```powershell
cargo build --locked --features json --bin vlite-graph-cli
# or install it into ~/.cargo/bin from the checkout
cargo install --locked --path . --features json --bin vlite-graph-cli
```

Usage: `vlite-graph-cli --db PATH [--writable] [--explain] COMMAND [REQUEST]`.
`REQUEST` is one JSON object; `-` reads it from stdin (at most 64 MiB) and
omitting it means `{}`. Each command maps to one `vlite::property_graph` call.
`query` takes one read-only [graph query text](GRAPH-QUERY.md) that translates
to exactly one typed read; it adds no other capability.

| Command | Access | Library call |
|---------|--------|--------------|
| `graphs` | read | `list_graphs` |
| `get-node`, `get-edge`, `find-nodes`, `neighbors`, `traverse`, `shortest-path` | read | `GraphRef::query` (with `--explain`: `GraphRef::explain`) |
| `query` | read | `GraphRef::query_text` (with `--explain`: `GraphRef::explain_text`) |
| `init` | write | `Database::create` (new file only) |
| `create-graph`, `drop-graph` | write | `create_graph`, `drop_graph` (empty graphs only) |
| `batch` | write | `GraphMut::write_batch` (one atomic frame) |

```powershell
vlite-graph-cli --db kb.vlite --writable init '{"dimensions":3}'
vlite-graph-cli --db kb.vlite --writable create-graph '{"graph":"kb"}'
Get-Content ops.json | vlite-graph-cli --db kb.vlite --writable batch -
vlite-graph-cli --db kb.vlite graphs
vlite-graph-cli --db kb.vlite neighbors '{"graph":"kb","node":{"key":"api"},"direction":"both"}'
vlite-graph-cli --db kb.vlite --explain traverse '{"graph":"kb","start":[{"key":"api"}],"limits":{"max_hops":3}}'
vlite-graph-cli --db kb.vlite query '{"graph":"kb","query":"MATCH (a KEY $k)-[:READS*1..3]->(b:Database) RETURN b LIMIT 20","params":{"k":{"type":"string","value":"api"}}}'
```

`ops.json` holds one batch of at most 4,096 operations. They apply in order and
commit together or not at all:

```json
{"graph":"kb","ops":[
  {"op":"create_node","key":"api","labels":["Service"],"properties":{"tier":{"type":"integer","value":"1"}}},
  {"op":"create_node","key":"db","labels":["Database"]},
  {"op":"create_edge","key":"api-db","source":"api","relationship":"READS","destination":"db"},
  {"op":"put_vector","collection":"default","id":"7","vector":[1,0,0],"payload":"runbook","metadata":{}},
  {"op":"attach","node":"api","collection":"default","record":"7"}
]}
```

Batch operations: `create_node`, `upsert_node`, `patch_node` (`add_labels`,
`remove_labels`, `set`, `unset`), `delete_node`, `create_edge`, `patch_edge`,
`delete_edge`, `attach`, `detach`, `put_vector` (`Mutation::PutWithMetadata`
when `metadata` is given, otherwise `Mutation::Put`; `payload` is UTF-8 text)
and `delete_vector`. Nodes are addressed by key, edges by `{"id":...}` or
`{"key":...}`. `expected` is `"any"` (default), `"absent"` or
`{"revision":"N"}`.

- **Access.** Reads use `Database::open_read_only` (shared lock), never write,
  and fail with `locked` while a writer owns the file. Writes require
  `--writable` and take the exclusive lock for one command; they fail with
  `locked` while any other handle is open. `--writable` on a read, or its
  absence on a write, is a `usage` error.
- **Graph-enabled files only.** Files are never converted. Graph commands on a
  vector-only file fail with `graphs_not_enabled`, and `init` refuses an
  existing path. Copy a v4/v5 file into a graph-enabled one with
  `vlite-maintain enable-graph SOURCE DESTINATION`.
  Encrypted files are not supported.
- **Bounded reads.** Queries accept `"limits"` (fields of `GraphLimits`;
  omitted fields use defaults; values above the ceiling fail with
  `graph_limit_exceeds_ceiling` and are never clamped) and `"projection"`
  (`"all"`, `"none"` or `{"only":[...]}`). Every result carries `graph`,
  `view_sequence`, `complete`, `truncation`, `stats` and `cursor`. Pass a
  returned lowercase-hex `cursor` with the same request to continue
  `find-nodes` or `neighbors`; a write in between fails with
  `graph_cursor_stale`. `--explain` returns the normalized query, effective
  limits, projection and index choices without reading nodes or edges.
- **Query text.** `query` takes `{"graph","query","params","limits"}`;
  `params` binds `$name` values and a missing or unused one is an error.
  `RETURN`, a `*1..N` hop range and `LIMIT` set the projection, `max_hops` and
  `max_output_nodes`; output equals the equivalent typed command. Text errors
  (`graph_query_syntax`, `graph_query_unsupported`, `graph_query_parameter`,
  `graph_query_limit`) name the byte offset and occur before the file opens.
- **Output.** Success prints one JSON object on stdout and exits 0. Failure
  prints `{"error":{"code":"...","message":"..."}}` on stderr and exits 1.
  Codes include `usage`, `invalid_request` (malformed JSON, unknown or repeated
  fields, keys or operations, malformed IDs or cursors), `invalid_input`
  (values the library rejects, such as invalid keys, labels, query bounds or a
  cursor from another query), the REST graph codes (for example
  `graph_not_found`, `graph_node_not_found`, `graph_attached_record`),
  `locked`, `precondition_failed` and `io_error`. IDs, revisions and sequences
  are decimal strings in output and accepted as strings or JSON integers, so
  values up to 2^64-1 keep full precision. Counts and limits are numbers.
  Enumerations use `"name"` or `{"name":{...}}`.

## Tests

Tool tests use small fixtures generated in the test: CSV, JSONL, COPY, both
FAISS flat metrics and, with `import-formats`, Arrow and Parquet.

```powershell
cargo test --locked --features tools --test shell_cli --test import_tools --test tune_cli
cargo test --locked --all-features --test shell_cli --test import_tools --test tune_cli
cargo test --locked --features json --test graph_cli
```

Contributors validate a full change with:

```text
cargo test --locked --all-targets --all-features
cargo test --locked -p vlite-graph
cargo test --locked --test graph_storage --test graph_compat --test graph_traversal --test graph_maintenance --test graph_recovery
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --check
docker compose --profile test run --build --rm tests
python3 tests/rest_e2e.py
python3 tests/tls_e2e.py
```

Browser acceptance tests need Playwright on the Docker host (inside WSL when
Docker runs there):

```text
python3 -m pip install -r tests/requirements-dashboard.txt
python3 -m playwright install --with-deps chromium
python3 tests/dashboard_e2e.py
```

Website and documentation checks live in `tests/site_links.py`,
`tests/release_docs.py` and `tests/content_budget.py`. See
[development and testing](REFERENCE.md#development-and-testing) for what each
suite covers, including the seeded crash-recovery histories.
