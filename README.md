# VLite

VLite is an experimental embedded vector database written in Rust. It owns its
storage engine: it is not a SQLite wrapper. The embedded core supports one
fixed-dimension vector space per database, opaque payloads, durable single-record
and atomic batch mutations, and exact nearest-neighbor search.

REST, gRPC, ANN indexes, distributed operation, interactive transactions,
encryption, metadata filtering, and read-only/multi-reader handles are deferred.
This is a correctness-first MVP, not a production-hardened storage engine.

## Build and use

Requires Rust 1.89 or newer. Maintenance uses `tempfile` for private staging and
no-clobber publication; the vector storage format remains VLite's own engine.

```powershell
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo run --example basic -- demo.vlite
```

The example creates a **new** database and refuses to overwrite an existing file.
When consuming this source locally, add a Cargo path dependency:

```toml
[dependencies]
vlite = { path = 'Q:\VLite' }
```

```rust
use vlite::Database;

fn main() -> vlite::Result<()> {
    let mut db = Database::create("vectors.vlite", 3)?;
    db.put(10, &[1.0, 0.0, 0.0], b"document A")?;
    db.put(20, &[0.0, 1.0, 0.0], b"document B")?;
    let hits = db.search(&[0.9, 0.1, 0.0], 5)?;
    assert_eq!(hits[0].id, 10);
    assert_eq!(db.get(10)?.unwrap().payload, b"document A");
    assert!(db.delete(20)?);
    db.close()?;

    let db = Database::open("vectors.vlite")?;
    assert_eq!(db.len(), 1);
    db.close()
}
```

## Docker end-to-end testing

Requires Docker Engine with **Linux containers** and Docker Compose v2 or newer.
These commands work from the project root on Linux, macOS with a Linux Docker
VM, or Windows with Docker Desktop/WSL. Docker runs Linux code on all these
hosts; it does not replace testing the native Windows or macOS filesystem APIs.
No network API, published ports, or long-running server are included.

Build and run the persistent smoke test:

```text
docker compose build vlite
docker compose run --rm vlite
docker compose run --rm vlite
```

On the first invocation, `vlite-smoke` launches a separate process that creates
`/data/smoke.vlite`, atomically inserts vectors and payloads, updates a record, and
deletes a record in one batch. A second process reopens the file and verifies every expected record and
exact-search result. The second container invocation verifies the **same
persisted file** without changing it. Successful runs print `PASS` and exit 0;
any error or incorrect result prints `FAIL` and exits nonzero. Existing corrupt
or unexpected databases are never reset or repaired automatically.

The runtime uses a non-root UID/GID of 10001, a read-only root filesystem,
dropped capabilities, and a Docker-managed `vlite-data` named volume for writable
storage. No host-directory permissions need to be configured. The container is
intentionally a finite test job: exiting successfully is expected, not a crash.
For an exit-code-driven orchestration run:

```text
docker compose up --build --abort-on-container-exit --exit-code-from vlite vlite
```

Run the entire Rust test suite, including corruption, interrupted-write,
subprocess recovery, locking, and CLI tests, in the development/test image:

```text
docker compose --profile test run --build --rm tests
```

Every runtime image build also runs the complete suite before building the
release smoke executable. Builds use the checked-in Cargo lockfile and Rust 1.89;
the base-image tags can receive upstream updates, so bit-for-bit image
reproducibility is not claimed. The runtime image contains no Cargo or compiler.

Without Compose, equivalent persistent smoke commands are:

```text
docker build --target runtime -t vlite-smoke:local .
docker volume create vlite-smoke-data
docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges --mount type=volume,src=vlite-smoke-data,dst=/data vlite-smoke:local
docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges --mount type=volume,src=vlite-smoke-data,dst=/data vlite-smoke:local verify /data/smoke.vlite
```

The volume survives container removal and `docker compose down`. To start an
independent test without deleting existing data, use a different Compose project,
for example `docker compose -p vlite-fresh run --build --rm vlite`. Volume removal
is destructive; do not remove volumes containing databases you want to retain.
The smoke file is a reserved fixture, not a generic database CLI.

For a WSL-only Docker installation, enter the WSL distribution and change into
the mounted project directory before running these commands. On this development
machine that is `cd /mnt/q/VLite`. Prefer Docker-managed local volumes over host
bind mounts, network shares, and cloud-synced folders for locking and sync tests.
The default images build for the engine's architecture; Linux amd64 and arm64 are
intended targets where the upstream Rust/Debian images are available.

The same process-separated smoke test is available natively:

```text
cargo run --bin vlite-smoke -- run smoke.vlite
cargo run --bin vlite-smoke -- verify smoke.vlite
```

IDs are `u64`, including zero. `put` replaces both the vector and payload for an
existing ID. `get` returns `None` for missing IDs; `delete` returns `false` without
writing for a missing ID. Dimensions are fixed at creation, from 1 through 65,536.
Vectors contain finite `f32` values; NaN and infinities are rejected. Payloads are
uninterpreted bytes limited to 1,048,576 bytes per record, including empty payloads.

Search uses **squared Euclidean distance**, accumulating in `f64`. It scans every
live vector, sorts by distance, and breaks ties by ascending ID. `k=0` returns no
hits; `k` above the live count returns every record. Query validation still applies
when `k=0` or the database is empty. Normalize vectors yourself if your application
needs normalized-vector semantics; no cosine or embedding-generation API exists.

## Atomic batch writes

New databases use format v2. A batch is one bounded checksummed commit frame,
not a loop over separately acknowledged writes:

```rust
use vlite::{Database, Mutation};

fn main() -> vlite::Result<()> {
    let mut db = Database::create("batch.vlite", 2)?;
    db.write_batch(&[
        Mutation::Put {
            id: 10,
            vector: vec![1.0, 2.0],
            payload: b"document A".to_vec(),
        },
        Mutation::Put {
            id: 20,
            vector: vec![3.0, 4.0],
            payload: Vec::new(),
        },
        Mutation::Delete { id: 10 },
    ])?;
    assert!(db.get(10)?.is_none());
    assert!(db.get(20)?.is_some());
    db.close()
}
```

All operations are validated before writing anything. They execute in slice
order; the last mutation of a repeated ID wins. A missing delete is a no-op but
still part of a nonempty batch's commit. Empty batches do not write or sync.
Limits are **4,096 operations** and **16 MiB of encoded body bytes**, including
16 bytes of metadata per operation plus put vectors/payloads. Existing dimension,
finite-component and per-record payload rules still apply. Oversized batches
return `BatchTooLarge`; invalid input never commits a valid prefix.

One `sync_all()` acknowledges the entire batch before any live-map change.
Reopen validates the whole frame and all entries before replaying the batch.
An interrupted suffix never exposes a partial batch: open fails closed, and
explicit tail repair discards the entire incomplete frame, subject to the same
potential loss of externally truncated acknowledged data described below.
A failed write or sync poisons the handle and leaves the **whole batch**
indeterminate: it can be absent or fully present after reopening, not partially
applied. There are no begin/commit sessions, cross-process snapshots, retry
tokens, or unbounded transactions. Application allocations can still abort the
process on OOM.

**Existing v1 databases:** reads and single-record writes remain supported,
with their original encoding unchanged. Nonempty batches return
`BatchRequiresUpgrade`; the library never rewrites a v1 header in place. Close
the v1 database and compact it to a **new** path to explicitly upgrade to v2,
then verify and switch paths. Backup/restore preserve the input format exactly.
Old VLite binaries reject a v2 header as unsupported; no backward-reading claim
is made. `Database::format_version()` and maintenance inspection expose the
version.

## Reproducible benchmark

`vlite-bench` generates deterministic fixed-point vectors using SplitMix64,
ingests them with atomic batches and sync-before-success, closes and reopens
the database, warms up one query, and times exact squared-L2 queries. Every
query is checked against an independent brute-force calculation on the original
generated vectors **outside** the measured query time. Incorrect results make
the benchmark fail rather than print successful metrics.

```text
cargo run --release --bin vlite-bench -- 1000 32 100 100 10 42
docker compose run --build --rm --entrypoint vlite-bench vlite 1000 32 100 100 10 42
```

Arguments are `records dimensions queries batch_size k seed`, in that order,
with the shown defaults. `batch_size=1` still uses the batch frame API, allowing
a controlled sync-per-record versus larger-batch comparison. The seed and
dataset checksum identify identical inputs across platforms. A private
temporary database is created under the current directory (Docker: `/data`),
or `VLITE_BENCH_DIR` when set, and is removed on success or ordinary errors.
There is no overwrite or destructive cleanup of existing databases. Abrupt exit
can leave temporary benchmark data; cleanup is then an explicit user decision.

Output is one JSON object with schema version 1, engine/file-format versions, OS/architecture,
debug/release profile, all inputs, hexadecimal dataset checksum, ingestion seconds and
vectors/second, reopen milliseconds, nearest-rank query p50/p95 milliseconds,
database bytes, source/query vector-data byte counts and recall@k. With the
current exact engine, recall@k must be 1.0; this is a correctness check, **not**
an ANN performance claim. Distances and tied IDs must also match the oracle.
`process_peak_rss_bytes` is explicitly `null`: vector-data counts are not
process-memory measurements. Use an external process-memory profiler when
comparing actual memory consumption.

Ingestion timing includes operation construction, encoding, writes, sync and
live-map updates, but excludes dataset generation, header creation and reopen.
Reopen timing includes file open, locking and full replay. Query timings exclude
the oracle and warmup, include result allocation/sorting, and are sequential
single-process measurements. Reopen is a **warm-cache** measurement, not a
cold-disk claim. Filesystem/device, CPU, compiler, background load, dataset,
batch size and build profile affect results; record those alongside outputs.
Do not compare this synthetic workload directly with another project's
published benchmark or infer physical power-loss safety from throughput.

Bounds reject invalid workloads before creating files: at most 1,000,000
records, 10,000 queries, 256 MiB of input vector data and 500 million query
distance components, plus the library's dimension and batch limits. Query k
must be between 1 and the record count. These bounds are not a peak-RAM or
execution-time guarantee.

## Storage maintenance

`vlite-maintain` provides offline inspection, backup, restore-to-new-path and
copy-out compaction. Stop every database owner before running it; every operation
holds the source's exclusive OS lock and returns `Locked` rather than waiting.
The source is opened read-only. Inspection validates the entire file; it is not
a quick header-only check.

```text
cargo run --bin vlite-maintain -- inspect vectors.vlite
cargo run --bin vlite-maintain -- backup vectors.vlite backup.vlite
cargo run --bin vlite-maintain -- restore backup.vlite restored.vlite
cargo run --bin vlite-maintain -- compact vectors.vlite compact.vlite
```

Library equivalents are `vlite::maintenance::{inspect, backup, restore, compact}`.
Inspection returns format version, dimensions, live records, validated commit-frame count, file
bytes, valid-prefix bytes and incomplete-tail bytes. The CLI prints these as
`key=value` fields. Corruption returns an error; an incomplete suffix is reported
by the library and makes CLI inspection exit nonzero. No inspection repairs or
changes data.

Backup and restore produce byte-identical, fully validated v1 or v2 files. Restore
**never overwrites an existing database**. Both reject corruption and incomplete
tails. Compaction writes only current live records, removes tombstones and old
values, writes format v2, and resets the output log sequence to start at one. It preserves IDs,
dimensions, payloads and vector component bits, including negative zero. Exact
search behavior is unchanged. An empty live set produces a 32-byte file that can
accept new writes. Inspection's commit count is a file-local frame count, not an
unchangeable lifetime mutation counter; a batch counts as one commit frame.

Output is first written to a private temporary file in the destination directory,
fully validated and synced, then published using no-clobber semantics. Existing
files, directories, and aliases to the source are never replaced. A normal error
before publication removes staging data; abrupt process termination can leave
an orphan temporary file. No automatic cleanup scans or repair run at startup.
Only the final destination name denotes a successfully published output.

Unix additionally syncs the destination directory after publication. If this
fails, `PublicationIndeterminate` identifies the final path: a complete file
already exists, but directory-entry durability is uncertain. Inspect it before
retrying rather than deleting or overwriting it. Windows output contents are
synced before publication, but directory-entry persistence across physical power
loss is not guaranteed. Filesystems must support the staging helper's
no-clobber publication; unsupported operations fail explicitly. The usual
hardware, sync, and cooperative-lock limitations below still apply.

Compaction is intentionally **copy-out, not in-place**. To reclaim space safely:
stop all owners; compact to a new path; inspect it and verify application data;
change your application's configured database path; retain the original until
you have verified the new database and backups. Delete the old file only when you
explicitly decide it is no longer needed. Do not rename over an open database.
Compaction temporarily needs space for both files and RAM for the live record
map; it does not immediately shrink the source.

The Docker runtime contains smoke, maintenance and benchmark executables. After creating the smoke fixture,
maintenance can be tested across containers using the same volume:

```text
docker compose run --build --rm vlite
docker compose run --rm --entrypoint vlite-maintain vlite inspect /data/smoke.vlite
docker compose run --rm --entrypoint vlite-maintain vlite backup /data/smoke.vlite /data/backup.vlite
docker compose run --rm --entrypoint vlite-maintain vlite restore /data/backup.vlite /data/restored.vlite
docker compose run --rm --entrypoint vlite-maintain vlite compact /data/restored.vlite /data/compact.vlite
docker compose run --rm vlite verify /data/compact.vlite
```

Rerunning copy commands with the same destinations fails intentionally. Use new
names or a separate Compose project for a fresh run; do not delete real backups
merely to rerun examples. Staged outputs use restrictive `tempfile` permissions
(owner-only on Unix); share backups only through explicitly managed OS access.
Backups are not encrypted and inherit the same sensitive payloads as the source.

## One file and ownership

Each database is exactly one self-contained file. This version creates **no WAL,
journal, index, or lock sidecars**. Maintenance briefly creates a private staging
file in the destination directory before publication. The append log is the canonical store;
the complete live-vector map is rebuilt in RAM on every open. Future versions
may introduce explicitly documented transient sidecars, but none are required
to move a closed database of either supported version.

An exclusive, nonblocking OS file lock is held for the handle's entire lifetime.
Even another reader must wait until the owner closes. Attempts to open or repair
an owned database return `Error::Locked`. Threads may serialize access using a
`Mutex<Database>`; there are no background workers. `close()` consumes the handle;
dropping it also releases the lock and requires no commit-on-drop.

Use local filesystems that honor OS file locks and flush requests. Do not use NFS,
SMB, cloud-synced directories, multiple hard-link/symlink aliases, or concurrent
external file editors. Do not rename, replace, copy, truncate, or otherwise modify
an open database. For backup or transfer, close every handle first and copy the
single file. Locks are cooperative on some platforms and do not prevent hostile
or noncooperating software from modifying the file.

## Commits, recovery, and durability

Each `put`, successful `delete`, or nonempty `write_batch` appends one checksummed frame with a commit
footer, then calls `File::sync_all()`. Only after sync succeeds does the in-memory
state change and the operation return success. There is no relaxed durability
mode. A subsequent open validates all headers, record lengths, sequence numbers,
checksums, and commit footers before exposing any vectors.

| Situation | Behavior |
|---|---|
| Complete valid frames | Replay in sequence; newest record or tombstone wins. |
| Interrupted final append | `open` returns `IncompleteTail`, without modifying the file. |
| Full frame with invalid checksum/footer, invalid lengths, bad sequence, or invalid header | `Corrupt`; neither open nor tail repair discards it. |
| Unsupported format version with a valid header checksum | `UnsupportedVersion`; no automatic migration. |
| Commit write or sync fails | `CommitIndeterminate`; reads/search/writes on that handle then return `Poisoned`. Close and inspect/reopen. |
| Repair truncation or sync fails | `RepairIndeterminate`; inspect/reopen before retrying. |
| Creation fails after creating the path | Error; a partial file may remain and is never silently overwritten. |

A failed operation is **not** evidence that the mutation is absent. A complete
frame may have reached storage before an error or process exit; reopening can
therefore reveal an unacknowledged operation. Reconcile by ID after reopening;
batch retry is also indeterminate and there are no retry tokens.

An incomplete suffix is not silently dropped. After backing up the **closed**
file, an application can explicitly authorize destructive tail repair:

```rust,no_run
let report = vlite::Database::discard_incomplete_tail("vectors.vlite")?;
println!("discarded {} bytes", report.discarded_bytes);
let db = vlite::Database::open("vectors.vlite")?;
# Ok::<(), vlite::Error>(())
```

Repair validates the entire complete prefix and truncates only a structurally
incomplete final frame. It returns the discarded byte count and retained commit
count; a clean file is a no-op. It never skips a complete bad frame or searches
past corruption.

**Important limits:** truncated acknowledged data can look exactly like an
interrupted unacknowledged write. Explicit repair accepts potential loss of that
last truncated commit. Removing entire frames from the end of a file is
undetectable without an external trusted history. Checksums do not authenticate
data and cannot detect every possible storage fault. There is no automatic
fallback to an older snapshot that silently discards a known committed frame.

Durability depends on the OS, filesystem, device, and controller actually honoring
sync. The library does not guarantee against hardware lies, media failure, power
loss on unsupported storage, or external file modification. `create` syncs file
contents but does **not** portably sync its parent directory: persistence of a
new filename across machine power failure requires platform-specific directory
durability handling by the caller. Tests simulate interrupted writes and process
exit, not physical power loss.

## File formats v1 and v2

All integers and IEEE-754 `f32` components use little-endian encoding, never native
Rust struct layouts. Checksums are IEEE CRC-32 (reflected polynomial `0xEDB88320`,
initial/final XOR `0xFFFFFFFF`, check value `0xCBF43926` for `123456789`).

The 32-byte database header:

| Offset | Size | Value |
|---|---|---|
| 0 | 8 | `VLITEDB` followed by NUL |
| 8 | 4 | Format version, 1 or 2; new files use 2 |
| 12 | 4 | Dimensions |
| 16 | 12 | Reserved, zero |
| 28 | 4 | CRC-32 of bytes 0 through 27 |

Every single-record mutation consists of a 36-byte record header, body, and 16-byte commit
footer:

| Offset | Size | Value |
|---|---|---|
| 0 | 4 | `VREC` |
| 4 | 1 | Operation: 1=put, 2=delete |
| 5 | 3 | Reserved, zero |
| 8 | 8 | Sequence, starting at 1 and increasing by exactly 1 |
| 16 | 8 | ID |
| 24 | 4 | Vector component count |
| 28 | 4 | Payload byte count |
| 32 | 4 | CRC-32 of record header bytes 0 through 31 |
| 36 | variable | Vector `f32` bytes followed by payload bytes |
| after body | 4 | `VCMT` |
| after body + 4 | 4 | CRC-32 of the complete record header and body |
| after body + 8 | 8 | Repeated sequence |

Put component counts must equal database dimensions; deletes have zero components
and zero payload bytes. Length limits and header CRCs are checked before allocating
record bodies. Unknown operations and nonzero reserved fields are rejected.
Replaying a tombstone removes that ID. Sequence exhaustion rejects the next write.
The immutable header is never updated after creation.

Version 2 additionally supports operation **3=batch** in the same outer frame
layout. Outer ID (offset 16) must be zero, offset 24 is the operation count and
offset 28 is the **total encoded body byte length**, not payload bytes. The
header checksum, `VCMT` footer, whole-frame checksum and repeated sequence work
as above. One batch advances the sequence by one.

The body is exactly the declared number of consecutive entries:

| Entry offset | Size | Value |
|---|---|---|
| 0 | 1 | Operation: 1=put, 2=delete; nested batches forbidden |
| 1 | 3 | Reserved, zero |
| 4 | 8 | ID |
| 12 | 4 | Payload length; zero for delete |
| 16 | variable | Put: database-dimension count of f32 values then payload; delete: empty |

Both batch count and encoded byte limits are checked before allocating the
outer body. Entries must fit entirely, contain valid finite vectors and satisfy
payload limits; trailing extra bytes are rejected. All entry checks happen
before applying any operation. Version 1 readers in this release reject batch
records inside v1 files as corruption rather than interpreting unknown data.

Fixed fixtures `tests/fixtures/v1.hex` and `tests/fixtures/v2.hex` were encoded
independently with Python `struct` and `zlib.crc32`, not generated by the Rust
writer during tests. Tests pin complete legacy/current file bytes, replay and
single-write compatibility, current batch encoding and explicit copy-out
upgrade. The fixtures are format contracts: do not regenerate them to conceal
an incompatible writer change. New incompatible frame meanings require a new
format version and explicit migration; reserved bytes and operation numbers
must never be silently repurposed under an existing version.

## Capacity and platform status

Space grows with **all historical mutations**, not just live records; deletes do
not reclaim disk space until you switch to a compacted copy and retire the old
file. Open time is linear in log size, memory is proportional
to live vectors/payloads, and search costs O(n*d + n*log(n)) time plus O(n) result
memory. There is no total file or RAM quota and allocation exhaustion is not a
recoverable database error. Plan capacity accordingly.

The source uses portable Rust I/O, standard OS locks, and explicit binary
encoding, targeting Windows, Linux, and macOS. Windows x86_64 (MSVC) has been
tested with Rust 1.99.1 and the declared Rust 1.89.0 minimum. The Linux amd64
Docker image is tested using Docker Engine in Ubuntu 24.04 WSL, including
non-root volume persistence across separate containers and the full test suite.
The previous maintenance release's full test suite passed in native Windows,
Linux and macOS GitHub Actions jobs on minimum and stable Rust, along with lint
and Linux Docker end-to-end checks. The same CI matrix runs the batch,
compatibility and benchmark tests for this milestone. Docker on macOS and Linux arm64 images have not been tested.
Container testing does not verify physical power-loss durability. No on-disk compatibility
with SQLite, redb, or Qdrant is claimed; their storage/ownership/index-separation
designs are conceptual references only.
