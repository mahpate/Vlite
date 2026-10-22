# Offline command-line tools

Build the shell, import, tuning and quality tools with Rust 1.89 or later:

```powershell
cargo build --locked --features tools --bins
# Optional Parquet and Arrow IPC readers (Arrow/Parquet 56.2, MSRV 1.84):
cargo build --locked --features import-formats --bins
```

`tools` enables JSON and CSV; `import-formats` additionally enables optional
Arrow/Parquet dependencies. Neither is enabled in the default embedded build.
Each tool supports `--help`; unknown/duplicate options and failures exit nonzero.
All tools use public embedded APIs, acquire the engine's exclusive file lock,
and require the database's server/other owner to be stopped first.

## Interactive or scripted shell

```powershell
vlite-shell --db example.vlite --create 3
vlite-shell --db example.vlite --read-only --script commands.txt
Get-Content commands.txt | vlite-shell --db example.vlite --read-only
```

`--create N` explicitly creates a new format-v4 database and refuses an existing
path. Without it, open requires an existing database. Commands are one per line:

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

Search supports the library's typed filters and exact metrics (`squared_l2`,
`cosine`, `inner_product`); ANN supports L2/cosine, not inner product. JSON consumes
the rest of the line, so strings may contain spaces without shell quoting.
`#` at the start of a line is a comment. Prompts appear only with terminal stdin.
Scripts/stdin fail at the first command error; earlier successful writes remain
committed. Interactive command errors are printed and the REPL continues.

`--read-only` is a **tool-level safety guard**: forbids put/delete/create and
copy-out maintenance; ANN/rebuild only changes in-memory indexes. It does not
provide concurrent readers or an OS read-only database handle: the public engine
open API still requests read/write permissions. `verify` uses the engine's
read-only maintenance API, releasing/reopening the database handle around it.
Header/stats describe the selected collection plus file format/size, not raw
header bytes. Browse uses the public bounded, ascending-ID page API: `browse LIMIT [AFTER_ID]`
accepts 1–1,000 rows and prints `next_after` for continuation. Payloads
are displayed as lossy UTF-8; writes accept UTF-8 strings.

## Validated, atomic or resumable imports

```powershell
vlite-import --db new.vlite --create 3 --input rows.jsonl --format jsonl
vlite-import --db new.vlite --input embeddings.csv --format pgvector
vlite-import --db new.vlite --input vectors.parquet --format parquet
vlite-import --db new.vlite --input flat.faiss --format faiss
# Explicitly opt into chunk-level atomicity and resume using the SAME input/options:
vlite-import --db new.vlite --input large.jsonl --format jsonl --chunk-size 1000 --job 42
```

Default mode reads and validates the **whole input** before submitting one
`RejectChunk` import through the existing v4 durable checkpoint API. A parse,
dimension, nonfinite vector, payload/metadata validation or frame-size error
publishes no rows. Invalid text rows are counted with row numbers (first 20
syntax errors shown); rejected engine rows report stable error codes. Existing
v1–v3 files must first be explicitly upgraded using `vlite-maintain upgrade`.
Named collections can be selected with `--collection NAME` and must already exist.

The default atomic maximum is conservatively 4,095 rows (one slot reserved for a
checkpoint), also bounded by the engine's 16 MiB frame budget. A conservative
byte-budget precheck may reject inputs just below the exact encoded limit.
Larger inputs require explicit `--chunk-size 1..4095`. All rows are parsed and
validated first even in chunked mode, but **I/O failures or checkpoint conflicts
can leave earlier chunks committed**. There is no whole-file transaction across
chunks. `--job` retries the last committed chunk with its engine digest and skips
earlier committed chunks; it can also replay a finished job without writes.
Keep the exact input, collection, chunk size and job ID unchanged for resume.
Only the last committed chunk's digest is retained by the public API: earlier
skipped chunks cannot be revalidated against their original contents. Aborted
jobs cannot resume. Empty inputs import zero rows without creating a job marker.
Success reports job ID, processed row count and chunks as JSON; on retry the row
count includes previously committed rows. Import parsing currently holds the
entire input in memory; this is not an unbounded streaming loader.

### Supported input schemas

- **JSONL:** one object per line with `id` (u64 JSON number), `vector` (float
  array), optional `payload` (UTF-8 string), and optional `metadata` (the engine's
  typed object). IDs above JavaScript's safe integer range must still be emitted
  as exact JSON integer tokens. Unknown fields fail validation.
- **CSV / pgvector CSV:** RFC CSV with header `id,vector` and optional `payload`
  and `metadata` columns. The vector is a quoted pgvector/JSON literal such as
  `"[1,2,3]"`; metadata is quoted typed JSON. Column order is arbitrary.
- **PostgreSQL COPY text (`--format copy`):** tab-separated `id`, vector literal,
  and optional payload. Optional `COPY ... FROM stdin;` header and `\.` terminator
  are recognized. Payload supports `\\`, `\t`, `\n`, `\r`, and `\N` (empty).
  Other escapes/column layouts/full SQL dumps are not supported.
- **Parquet:** Arrow-compatible columns `id` (uint64 or nonnegative int64),
  `vector` (list/large-list/fixed-size-list of float32/float64, or UTF-8 JSON
  array), optional `payload` and `metadata` UTF-8 columns. Null id/vector/elements
  are rejected; null optional columns are empty. Float64 vectors convert to f32
  and values overflowing f32 are rejected. Reader supports uncompressed/Snappy
  Parquet; other compression codecs are intentionally not enabled.
- **Arrow IPC:** file or stream framing, with the same schema as Parquet.
  Requires `import-formats`, as does Parquet. Compressed IPC is not supported.
- **FAISS flat:** native `write_index` little-endian, 64-bit layout, fourcc
  `IxF2` (IndexFlatL2) or `IxFI` (IndexFlatIP), trained flag, dimension/count,
  matching metric and 64-bit float-count followed by contiguous float32 vectors.
  Length, dimension and finiteness are checked; trailing data is rejected.
  IDs are assigned from zero and payload/metadata are empty. Import transfers
  **vectors**, not metric settings: choose L2 or inner-product at search time.
  No IVF/PQ/HNSW, IDMap, GPU, transforms, endian conversion or 32-bit size_t
  serialization. See [FAISS's serializer](https://github.com/facebookresearch/faiss/blob/main/faiss/impl/index_write.cpp).

Repeated IDs retain the engine's ordered upsert semantics; existing IDs are
replaced, not appended with duplicate identity. Output paths are never clobbered
by `--create`.

## Index tuning advisor

```powershell
vlite-tune --db example.vlite --m 8,16,32 --ef-construction 64,128 --ef-search 20,50,100 --k 10 --samples 8 --target-recall 0.95 --json
```

Sweeps up to 256 parameter combinations, builds deterministic L2 HNSW indexes
in memory, and uses the same seeded sample of stored vectors for every candidate.
Samples are bounded to 1–32, k to 1–100. The cheapest configuration meeting the
target recall@k is recommended by the explicit cost function
`mean_query_us + memory_weight * component_MiB` (`--memory-weight`, default 1).
A missing recommendation is JSON `null` and a nonzero exit status. Reports include
build time, mean/p95 query latency, recall, Jaccard overlap, rank correlation and
component byte estimates. Memory includes resident vectors and graph links, not
allocator, record payload/metadata or node-object overhead; it is not peak RSS.

Measurements are local wall-clock estimates, not promises. Stored-vector queries
include self-neighbors and can overestimate real-world recall. Evaluate production
queries separately; tiny datasets and single samples are not statistically robust.
Indexes are built for the full selected collection, not just the sample. No index
or settings are persisted and source bytes remain unchanged.

## Search quality and configuration/database comparison

```powershell
vlite-quality --db example.vlite --k 10 --samples 8 --json
vlite-quality --db example.vlite --m 8 --other-m 32 --other-ef-search 200 --json
vlite-quality --db before.vlite --other-db after.vlite --json
```

The existing `vlite-bench` already measures benchmark recall/latency, and
`CollectionRef::ann_quality` already offers budgeted seeded recall. Those features
are reused, not reimplemented as a new index: these tools add real-database input,
parameter sweeping, rank/overlap analysis and side-by-side comparisons. The engine
quality result and skipped reason are included alongside measured query metrics.
Comparison uses identical queries from the primary file; IDs must have matching
meaning in both files. Both sides are also measured against their own exact
baseline. Different dimensions fail explicitly. Sequential opening permits two
configurations of the same path without conflicting locks.

Recall@k is intersection/exact count; overlap is intersection/union (Jaccard).
Rank correlation is Pearson correlation of original rank positions on shared IDs
(Spearman for unique engine rank positions); fewer than two shared IDs yield
`null`, and the aggregate averages only defined correlations. It is not a
penalty-based correlation over missing IDs. Reports use L2; output is compact
JSON with `--json`, otherwise labeled human-readable summaries.

## Tests

```powershell
cargo test --locked --features tools --test shell_cli --test import_tools --test tune_cli
cargo test --locked --all-features --test shell_cli --test import_tools --test tune_cli
```

Fixtures are small and generated in-test: CSV, JSONL, COPY, both FAISS flat metric
headers, and (with `import-formats`) Arrow files/streams and Parquet float lists.
Tests cover read-only byte preservation, CRUD/filter/ANN shell scripts, malformed
input atomicity, resumable replay, parameter selection and config/file comparison.
