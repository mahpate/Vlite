# VLite

**An experimental single-file vector database for Rust applications.**

VLite combines a custom append-only storage engine, durable atomic batches,
typed metadata, exact similarity search, and optional in-memory HNSW search.
Embed the Rust library directly or run the authenticated REST service in Docker.
It is not a SQLite wrapper and does not require an external database.

> **Status:** experimental. Correctness and recovery are prioritized over scale.
> The live dataset resides in memory; the log is replayed on open. VLite is not
> a distributed database or a substitute for a production-readiness assessment.

## Capabilities

- **Single-file persistence:** versioned binary records, checksums, synchronous
  commits, exclusive locking, and explicit interrupted-write recovery.
- **Atomic mutations:** vector/payload/metadata replacement and deletion,
  individually or in batches of up to 4,096 operations.
- **Exact search:** squared Euclidean distance with f64 accumulation,
  deterministic distance/ID ordering, and bounded top-k candidate memory.
- **Metadata filters:** strict typed equality and inclusive numeric ranges,
  applied before exact top-k selection.
- **Approximate search:** deterministic, rebuildable HNSW with a bounded
  mutation buffer for small mixed read/write workloads.
- **Deployment:** authenticated REST, OpenAPI, JSON metrics, redacted request
  logs, non-root Docker images, and an optional HTTPS reverse proxy.
- **Maintenance:** offline inspection, byte-preserving backup/restore, and
  no-clobber copy-out compaction.

VLite is licensed under the GNU General Public License v3.0 only; see
[LICENSE](LICENSE). There is no gRPC, cosine/inner-product metric, distributed replication,
multi-reader handle, encryption at rest, or approximate metadata filtering.
See the [technical reference](docs/REFERENCE.md) for contracts and limits.

## Quick start: embedded Rust

Requires **Rust 1.89 or later**. The default library build has no HTTP runtime.
From a checkout:

```text
cargo run --locked --example basic -- demo.vlite
```

The example creates a new file and refuses to overwrite an existing one.
For another Rust project, use a local dependency:

```toml
[dependencies]
vlite = { path = "../VLite" }
```

```rust
use vlite::Database;

fn main() -> vlite::Result<()> {
    let mut db = Database::create("vectors.vlite", 3)?;
    db.put(10, &[1.0, 0.0, 0.0], b"document A")?;
    db.put(20, &[0.0, 1.0, 0.0], b"document B")?;

    let hits = db.search(&[0.9, 0.1, 0.0], 5)?;
    assert_eq!(hits[0].id, 10);
    db.close()
}
```

Use `Database::open` to reopen an existing file. Plain `put` replaces the
entire record and clears prior metadata. `put_with_metadata` replaces all
three components. IDs are u64; each database has one fixed vector dimension.

## Quick start: Docker REST service

Requires Docker Engine with **Linux containers**, Compose v2, and Python 3 for
secret generation. On Windows with Docker in WSL, run these commands inside WSL.
From the repository root:

```text
python3 -c "import pathlib,secrets; p=pathlib.Path('.secrets'); p.mkdir(mode=0o700,exist_ok=True); (p/'api-token').write_text(secrets.token_urlsafe(32),encoding='ascii')"
docker compose --profile api up --build -d api
```

Keep the secret directory private and allow the container's UID 10001 to read
the token file. The file is Git-ignored and excluded from image builds.
`VLITE_TOKEN_FILE` selects an existing secret; `VLITE_DIMENSIONS` defaults to 3.
The API is published only at **127.0.0.1:8080**. The named data volume survives
container removal and `docker compose down`.

PowerShell client example (run from the checkout containing the secret):

```powershell
$auth = @{ Authorization = "Bearer $((Get-Content .secrets\api-token -Raw).TrimEnd())" }
$base = 'http://127.0.0.1:8080'
Invoke-RestMethod "$base/ready" -Headers $auth
Invoke-RestMethod "$base/v1/vectors/10" -Headers $auth -Method Put -ContentType 'application/json' -Body '{"vector":[1,0,0]}'
Invoke-RestMethod "$base/v1/search" -Headers $auth -Method Post -ContentType 'application/json' -Body '{"vector":[0.9,0.1,0],"k":5}'
```

Use the same token value on the host where requests are sent; do not put tokens
in URLs or commit them. `/health` is public; other routes require bearer
authentication. Native server usage supports token files/environment or an
explicit loopback-only `--insecure-local` development mode.

The binary serves plain HTTP. Use the supplied
[HTTPS deployment](docs/REFERENCE.md#https-observability-and-releases) before
exposing it beyond loopback. One token grants full access; there are no roles
or per-client quotas. Read the
[REST contract](docs/REFERENCE.md#rest-service-optional) and
[OpenAPI specification](src/openapi.json) before integrating.

## Search and indexing

Exact search is the default. It evaluates eligible vectors and retains at most
`min(k, live records)` candidates. Filters support string, signed integer,
finite float, and boolean values without implicit type coercion.

HNSW is opt-in (`"mode":"approximate"` in REST). Small writes are merged through
a mutation buffer rather than forcing a complete rebuild on every write.
The buffer is capped at **256 distinct IDs / 8 MiB of vector bytes**. Overflow,
reopen, or changed index configuration causes a synchronous rebuild on the next
nonempty ANN query. It is approximate: recall and a full k results are not
guaranteed. Filters currently require exact mode.

See [metadata and filters](docs/REFERENCE.md#typed-metadata-and-exact-filtered-search)
and [HNSW behavior](docs/REFERENCE.md#optional-hnsw-search).

## Performance and comparisons

Performance numbers are workload-specific, not product rankings.

- [Benchmark methodology and cross-database instructions](benchmarks/README.md)
- [Published measurements and raw results](benchmarks/RESULTS.md)
- [Native benchmark reference](docs/REFERENCE.md#reproducible-benchmark)

The native benchmark verifies every exact query against an independent oracle.
The cross-database harness compares **REST with REST**, using identical seeded
vectors, filters, k, resource limits, warmups, and recall checks. Embedded
zvec/FAISS comparisons belong in a separate in-process track; mixing their
latency with network calls would be misleading. Real embeddings, concurrency,
larger dimensions and datasets, recall targets, storage, and memory need their
own measurements before making deployment decisions.

## Data safety and maintenance

Successful writes append a complete checksummed commit and synchronize the
file before acknowledgment. Hardware/filesystem sync behavior and parent
directory durability impose limitations; this is not a physical power-loss
guarantee. A failed commit poisons the handle because the outcome may be
indeterminate. Never blindly retry an uncertain write.

Corruption fails closed. Incomplete trailing writes require explicit repair;
opening a database never silently discards committed data.

```text
cargo run --locked --bin vlite-maintain -- inspect vectors.vlite
cargo run --locked --bin vlite-maintain -- backup vectors.vlite backup.vlite
cargo run --locked --bin vlite-maintain -- compact vectors.vlite compact.vlite
```

Stop the database owner before maintenance. Output paths must be new; existing
files are never overwritten. Current files use format v3; legacy v1/v2 files
remain readable, but metadata writes require explicit copy-out migration.
See [maintenance](docs/REFERENCE.md#storage-maintenance),
[durability details](docs/REFERENCE.md), and the
[upgrade/rollback runbook](docs/REFERENCE.md#https-observability-and-releases).

## Development and validation

```text
cargo test --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --check
docker compose --profile test run --build --rm tests
python3 tests/rest_e2e.py
python3 tests/tls_e2e.py
```

CI covers Windows, Linux, and macOS on Rust minimum/stable, plus Linux Docker
REST persistence and verified HTTPS. Docker on a Windows/macOS host still
executes Linux code; it does not replace native filesystem testing. ARM release
publication is configured but not yet exercised.

The tag-triggered release workflow builds native archives/checksums and
multi-architecture container images, with acceptance gates. No published
release or GHCR image is implied by the presence of that workflow.

## Design references

VLite learns from separation of authoritative storage and rebuildable indexes,
including [zvec](https://github.com/alibaba/zvec)'s staged-index/optimization
model. Its storage format and Rust engine are independent implementations;
no zvec code is copied and no file-format compatibility is claimed.
Any future source reuse requires review of upstream licensing and notices.
This repository is licensed under the GNU General Public License v3.0 only.
See [LICENSE](../LICENSE) for the license terms.
