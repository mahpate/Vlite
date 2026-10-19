# VLite

**A single-file vector database for your applications, embedded or self-hosted.**

VLite combines a custom append-only storage engine, durable atomic batches,
typed metadata, exact similarity search, and optional in-memory HNSW search.
Use it in-process through native language bindings, embed the Rust library
directly, or connect to the authenticated REST service from any HTTP-capable
language.
It uses its own storage format and does not require an external database.

> **Operating model:** the live dataset resides in memory, and the log is
> replayed on open. One database owner serializes access to each file.
> Size deployments for the full dataset and validate recovery and workload
> requirements before production use.

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

VLite is licensed under the MIT License; see
[LICENSE](LICENSE). There is no gRPC, cosine/inner-product metric, distributed
replication, multi-reader handle, encryption at rest, or approximate metadata
filtering.
See the [technical reference](docs/REFERENCE.md) for contracts and limits.

## Choose how to use VLite

- **Python, embedded:** install a matching `vlite-embedded` platform wheel and
  open a `.vlite` file directly. No Rust, Docker or server is needed at runtime.
- **Node.js/TypeScript, embedded:** install a matching `@vlite/embedded` native
  tarball and call the engine asynchronously through a worker-backed addon.
  No HTTP service or Rust toolchain is needed at runtime.
- **Rust, embedded:** use the core crate directly in your application.
- **C/C++ and other native consumers:** use the versioned C ABI/shared library.
  Other languages can build bindings through their FFI; a dedicated SDK is
  not automatically provided for every language.
- **Python and TypeScript/JavaScript, REST:** use the first-party clients to
  connect to a self-hosted VLite service.
- **Other languages, REST:** call the documented HTTP/JSON API or generate a
  client from [OpenAPI](src/openapi.json). First-party C#, Java and Go SDKs
  are not currently included.

The engine's implementation language does not restrict the language of a
consuming application. Embedded packages must match their host's OS,
architecture and native runtime; REST clients only need network access to the
service. Embedded access still permits one owner per database file. Use the
service when multiple applications need to share one database.

Use [versioned release assets and installation instructions](docs/INSTALL.md)
for installation without cloning. Packages are not published to public package
registries. See the [embedded deployment guide](bindings/README.md)
and [REST client guide](clients/README.md) for installation and examples.

## Quick start: embedded Python

Install a matching platform wheel from the build artifacts with
`python -m pip install <wheel-path>`, then:

```python
from vlite_embedded import NativeLibrary

native = NativeLibrary()
with native.create("documents.vlite", dimensions=3) as db:
    db.put("10", [1, 0, 0])
    print(db.search([0.9, 0.1, 0], 5))
```

Use `native.open("documents.vlite")` to reopen; creation never overwrites.
See [Python installation and native platform support](bindings/README.md).

## Quick start: embedded Node.js / TypeScript

Install a matching native tarball using `npm install <tarball-path>`, then:

```javascript
import { NativeLibrary } from "@vlite/embedded";

const native = new NativeLibrary();
try {
  const db = await native.create("documents.vlite", 3);
  await db.put("10", [1, 0, 0]);
  console.log(await db.search([0.9, 0.1, 0], 5));
} finally {
  await native.close(); // Drains accepted work and closes owned databases.
}
```

These calls run native work in a dedicated worker thread. Node.js 22.9+ and
a matching native package are required; this is not a browser/edge SDK.
See [Node installation, TypeScript types and lifetime rules](bindings/node/README.md).

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
in URLs or commit them. `/health` and the static dashboard assets are public;
data, metrics and OpenAPI routes require bearer
authentication. Native server usage supports token files/environment or an
explicit loopback-only `--insecure-local` development mode.

## Embedded deployment across languages

Deploy VLite inside your application without a server using the native shared
library and versioned C ABI. Python has an embedded ctypes binding,
Node.js/TypeScript has an asynchronous native addon, and C/C++ uses the supplied
header. Other languages can build bindings against the same ABI.
Build once for the target OS/architecture; Rust is not required at runtime.
The database retains its single-owner file-locking and durability contracts.

See [embedded installation, Python examples and the C ABI](bindings/README.md).
Platform-specific Python wheels bundle the native library: install a matching
wheel and use `NativeLibrary()` without supplying a DLL path. Native wheel
builds and isolated installation checks are wired into CI/release workflows;
packages have not yet been published to a registry.
A dedicated audited manylinux wheel targets Linux x86_64/glibc 2.28+;
host-native wheels remain available for Windows and macOS.
The [embedded Node package](bindings/node/README.md) bundles its addon and
native library in host-specific, checksummed tarballs. Its Linux build is
host-specific, not covered by the Python wheel's manylinux certification.
The embedded binding supports atomic writes and bounded multi-record reads;
see [measured Python binding/batch performance](benchmarks/RESULTS.md#python-embedded-binding-and-batching-october-19-2026-utc).

## Python and TypeScript/JavaScript REST clients

First-party REST clients support vector CRUD, atomic batches, typed metadata,
all search modes, cursor browsing, index maintenance and metrics. Python 3.11+
uses the standard library; the Node.js 22+ ESM client includes TypeScript types
and has no runtime dependencies. Both preserve full-width IDs as strings,
expose structured errors and never automatically retry writes.

See [client installation, examples and error handling](clients/README.md).
Packages are currently installed from this checkout, not published registries.

## Browser dashboard

After starting the Docker API above, open **http://localhost:8080/dashboard**
and paste the existing `.secrets/api-token` value into the connection field.
No separate frontend container, Node service or network-hosted assets are needed.
The native REST binary includes the same dashboard.

The dashboard provides database information and metrics, paginated ID/metadata
browsing, click-to-inspect vector CRUD, payload/typed metadata editing, exact filters, all three search modes,
explicit HNSW builds, and a request/response inspector with browser-observed
latency. Put/delete/build operations require confirmation. It is a testing/admin
interface, not an embedding generator or production monitor.
Put replaces the entire record; no undo or automatic retries are provided.

Browse pages are capped at 100 records and 1 MiB JSON and omit vectors/payloads.
They are live views, not stable snapshots. HNSW readiness and buffer usage are
visible without attempting a search; refresh status after writes or builds.

Tokens stay only in page memory (not browser storage, cookies or URLs); reload
or Disconnect clears them. Requests use the same-origin bearer-protected API.
The public page contains no database contents or credentials. Use the HTTPS
overlay beyond loopback and treat the token as full read/write/admin access.
The inspector displays record data, so do not share screenshots containing
private vectors or payloads. Explicit builds still block the single owner.

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

For latency-sensitive callers, `Database::search_approximate_ready` (REST
`"mode":"approximate_ready"`) never builds an index during the query. Missing
or invalidated indexes return `IndexNotReady` / HTTP 409 `index_not_ready`;
there is no silent exact fallback. Schedule `build_hnsw` or authenticated
`POST /v1/index/hnsw` with `{}` separately. Builds still block the single owner,
so this separates maintenance from queries rather than providing a latency SLA.

See [metadata and filters](docs/REFERENCE.md#typed-metadata-and-exact-filtered-search)
and [HNSW behavior](docs/REFERENCE.md#optional-hnsw-search).

## Performance

Performance numbers are workload-specific, not product rankings.

- [Benchmark methodology and reproducible runs](benchmarks/README.md)
- [Published measurements and raw results](benchmarks/RESULTS.md)
- [Native benchmark reference](docs/REFERENCE.md#reproducible-benchmark)

Exact search can reject a candidate early when its partial squared-L2 distance
already exceeds the current top-k cutoff. Vectors above 32 dimensions check
the cutoff in 32-component blocks; short vectors retain the full-distance path.
This preserves exact distances and deterministic ties, but benefits depend on
the data and scan order. See the published cutoff comparison for gains and
regressions on the measured synthetic workloads.

The native benchmark verifies every exact query against an independent oracle.
It accepts external vector/query files and can measure durable mixed updates,
ANN rebuild pauses, raw latency samples, measured file growth and whole-process
peak memory through the native campaign runner.
Measure embedded calls and REST requests separately; mixing in-process latency
with network calls would be misleading. Use identical inputs, resource limits,
warmups and correctness checks when comparing runs. Real embeddings,
concurrency, larger dimensions and datasets, recall targets, storage, and memory
need their own measurements before making deployment decisions.

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

Browser acceptance tests additionally need Playwright on the Docker execution
host (WSL when Docker lives there). Use a virtual environment, then:

```text
python3 -m pip install -r tests/requirements-dashboard.txt
python3 -m playwright install --with-deps chromium
python3 tests/dashboard_e2e.py
```

This creates an isolated Docker database/container, exercises the UI with real
Chromium, removes the test container and retains its explicitly named data
volume. The runtime image itself needs neither Python nor a browser.

The bounded reliability campaign runs with `cargo test --locked --test reliability`.
Four fixed seeds compare mixed CRUD/atomic batches and exact search with an
independent state model across reopen, backup and compaction. Subprocess tests
forcibly terminate an owner after acknowledged writes and around a subsequent
unacknowledged batch, checking acknowledged-data retention, batch atomicity,
lock release and continued writes. Incomplete tails require an explicit
backed-up repair in the test; normal open must not modify the file.
The crash fixture is marked ignored because its parent invokes it explicitly.
Kill timing does not guarantee every write boundary is hit; these process-crash
checks complement byte-prefix recovery and sync-failure tests, not hardware
power-loss certification.

CI covers Windows, Linux, and macOS on Rust minimum/stable, plus Linux Docker
REST persistence and verified HTTPS. Docker on a Windows/macOS host still
executes Linux code; it does not replace native filesystem testing. ARM release
publication is configured but not yet exercised.

## Stable release criteria

The current package version is **0.1.0**. A stable release requires a verified
compatibility and operational baseline, not just a version or wording change:

1. **Freeze the supported contracts:** document the Rust API, REST/OpenAPI
   behavior, file-format compatibility, migration rules and support matrix.
   Define which changes require a major version increment.
2. **Pass release gates on the exact candidate:** run native minimum/stable Rust
   checks on Windows, Linux and macOS, plus Docker REST, HTTPS and browser
   acceptance. Verify each advertised image architecture before supporting it.
3. **Exercise recovery and upgrades:** verify acknowledged writes across
   restarts, interrupted-write and corruption handling, backup/restore,
   migration and rollback using retained compatibility fixtures.
4. **Set measured deployment limits:** run representative datasets and sustained
   mixed workloads; record latency tails, recall, memory, startup/replay time
   and file growth. Include index-buffer overflow and explicit rebuilds.
5. **Resolve distribution and security readiness:** verify license compliance, review
   dependencies and authentication/deployment configuration, and document
   supported versions and vulnerability reporting.
6. **Publish an immutable release:** update `Cargo.toml` and `Cargo.lock`
   together, record release notes and known limits, then tag the approved
   commit as `v<package-version>`. Use `v1.0.0` once the stable contracts above
   are satisfied; pin deployed images by digest.

The existing tag-triggered workflow builds native archives/checksums and
multi-architecture container images, with acceptance gates. A manual workflow
run can validate a candidate without publishing it. No published release or
registry image is implied by the presence of that workflow.

## License

VLite is licensed under the [MIT License](LICENSE). You may use, modify,
redistribute and include it in commercial products, provided you retain the
copyright and license notice. Contributions are welcome under the same license.
