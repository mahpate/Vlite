# Collections and advanced retrieval

These APIs extend the embedded engine. Existing `Database` methods continue to
operate on the `default` collection. Existing database files are never upgraded
automatically.

## File compatibility and collections

`Database::create` still creates format v3. To opt into named collections and
durable import checkpoints, create a new format v4 file:

```rust
use vlite::{CollectionSettings, CollectionSpec, Database, FormatVersion};

# fn example() -> vlite::Result<()> {
let mut db = Database::create_with_format("new.vlite", 3, FormatVersion::V4)?;
db.create_collection("documents", CollectionSpec {
    dimensions: 768,
    settings: CollectionSettings::default(),
})?;
db.collection_mut("documents")?.put(1, &vec![0.1; 768], b"document")?;
db.close()?;
# Ok(()) }
```

For a runnable example exercising collection creation, conditional revisions,
indexed planning, hybrid search, quantized index memory, import replay and reopen:

```powershell
cargo run --locked --example collections -- new-example.vlite
```

The example refuses an existing output file; it does not delete the new database.

Collections have independent dimensions, record IDs, settings and derived
indexes. Collection `default` cannot be dropped. A named collection must be
empty and have no open import job before it can be dropped. Limits and invalid
settings produce explicit errors; they do not create partial collections.

Older binaries cannot open v4 files. Upgrade to a **new, nonexistent path**:

```powershell
cargo run --bin vlite-maintain -- upgrade old.vlite new.vlite
cargo run --bin vlite-maintain -- verify new.vlite
```

Keep the original file. Stop its owner before offline operations. Verify and
explicitly select the new path before starting a service; upgrading a copy does
not switch a running service. Format-v4 restore and compaction assign a new file
identity. Derived indexes are rebuilt, not stored in the file.

Within a v4 lineage, collection IDs increase and are not recycled after drop.
Copy-out maintenance retains the allocation high-water mark. Once the ID space
is exhausted, creating another collection fails without writing, even if fewer
than the maximum live collections remain. A closed import retains its historical
collection name and a dropped flag; recreating the name cannot resume the old job.

This development change adds public error variants and marks `Error` as
non-exhaustive. Rust callers matching every prior variant must add a wildcard
arm; this is a source-compatibility change even though existing file formats
remain readable. No package release is implied by the working-tree changes.

## Record revisions

`revision(id)` returns the sequence of the record's last successful put.
Revisions are scoped to a collection and record; different records written in
one atomic batch can have the same revision. `Expected::Absent` and
`Expected::Revision(revision)` provide conditional puts/deletes.
Mismatches write nothing. Revision checks and writes use the same exclusively
owned database handle. Do not compare a bare revision across different files,
collections or IDs.

## Metadata indexes and automatic planning

Declare metadata indexes in `CollectionSettings::metadata_indexes` using
`MetadataIndexSpec` and `MetadataIndexKind::{Eq, Range, EqRange}`.
Indexes are maintained after successful commits and rebuilt on reopen or a
settings replacement. Candidate IDs are always rechecked against the complete
typed filter. Indexed exact results retain existing IDs, distances and ordering.

`CollectionRef::search_auto_with_settings(query, k, conditions, ef_search)`
returns hits, the chosen `QueryPlan` and `SearchStats`. Its strategies are
`exact_scan`, `exact_indexed`, `ann` and `filtered_ann`. Selective indexed
queries use exact candidate scoring; broad queries can use an already-ready ANN
index. Missing indexes use exact search. **Auto mode never builds an ANN index**.
Explicit exact/approximate modes keep their distinct contracts.

Collection settings persist in v4. Settings on a legacy default collection are
memory-only, explicitly reported by `SettingsPersistence::MemoryOnly`.

## Text and hybrid search

Declare up to eight string metadata fields in `CollectionSettings::text_fields`.
Opaque payload bytes are not text-indexed. Tokenization splits on nonalphanumeric
characters and lowercases tokens; there is no stemming, phrase search or
language-specific analyzer. Token, query and posting limits are enforced.
If the posting limit makes the index unavailable, durable writes still succeed
and text-dependent queries return an explicit readiness error.

`HybridQuery::new(Some(vector), text, k)` combines vector retrieval with BM25
using weighted reciprocal rank fusion. Both legs apply the same metadata filter.
Fusion scores are not vector distances: **higher scores are better**, with ID
tie-breaking. Results include leg ranks, optional vector distance and BM25 score.
`VectorLeg::ApproximateReady` never builds an index and can miss candidates;
the default vector leg is exact. Candidate depth and weights affect retrieval.

## Quantized ANN, memory and quality

Use `AnnOptions { quantization: Quantization::ScalarU8, rerank_multiplier: 4 }`
with the `*_with_options` APIs. Quantization is part of index readiness identity.
Queries using legacy APIs request an unquantized index, so they cannot silently
use an incompatible quantized graph. Traversal uses scalar-u8 codes; returned
candidates are reranked with exact distances from current f32 record vectors.
Buffered mutations and metadata eligibility still use current records.

`ann_memory` reports graph links, code/codebook, buffer and resident-vector
component bytes. These are **not measured process RSS**. Original f32 vectors
remain resident, so quantization does not promise a total-memory reduction and
can add code storage. `ann_quality` samples a ready index against exact search
under an explicit evaluation budget, returning an explicit skip reason when
sampling cannot run. Recall is workload-dependent, not guaranteed.

Reproduce a bounded release comparison:

```powershell
cargo build --locked --release --example search_capabilities_bench
python benchmarks\search_capabilities_campaign.py --binary target\release\examples\search_capabilities_bench.exe --output search-capabilities.json
python tests\search_capabilities_campaign.py
```

The output must not exist. The synthetic run compares both metrics with and
without quantization, verifies exact returned distances, records raw timings
and component bytes, and checks indexed exact results against the unindexed
baseline. Source/binary hashes and limitations accompany the results.

## Resumable ingestion

V4 `import_chunk(job, chunk, operations, policy)` atomically commits accepted
rows and a durable checkpoint in one synchronized frame. Replaying the latest
chunk with the same SHA-256 identity returns `AlreadyCommitted` without writing.
Different content, policy, a sequence gap or the wrong collection conflicts.
New job IDs increase monotonically; `next_import_job_id` allocates the next
candidate for the exclusively owned handle.

REST reservations exist only in memory until their first chunk commits. First
chunks must follow reservation order: a later job returns `import_conflict`
while an earlier reservation on a live collection remains uncommitted. Commit
or abort that earlier reservation before retrying; no chunk was written.

`RejectChunk` reports invalid rows and writes nothing. `SkipInvalid` commits
valid rows and reports skipped counts; detailed errors are bounded. An
all-skipped chunk still records its checkpoint. `finish_import` and `abort_import`
close a job; **abort does not roll back previously committed chunks**.
Legacy files return `ImportsRequireUpgrade`.

Each chunk is bounded by the existing 4,096-operation / 16-MiB batch limits,
including checkpoint overhead. Progress counters are full-width integers; compact
copies preserve open-job counters and latest replay identity. Closed-job status
retention is bounded and may be discarded by copy-out compaction; do not treat
the job registry as a permanent ingestion audit.

A failed write can leave an incomplete trailing frame and an indeterminate
outcome. The handle fails closed. Use the existing explicit incomplete-tail
repair workflow, reopen, inspect the checkpoint, then replay; do not assume a
transport or synchronization error means the chunk was not committed.

## REST service and scoped credentials

New databases can explicitly use `vlite-server ... --create-format 4` alongside
`--create-if-missing`; the default remains v3, and opening an existing file does
not migrate it. `GET /v1/capabilities` reports format, role and supported flags.
Collection routes are under `/v1/collections/{name}` and never silently fall
back to `default`. Legacy vector/search routes remain default-collection aliases
with their existing hard defaults; collection routes use collection settings.

REST settings updates merge supplied fields with current settings. The embedded
settings setter is a full replacement; do not assume these two request contracts
are interchangeable. All u64 record IDs, seeds and revisions use decimal strings.
Search `explain` adds work counters and, for auto mode, the selected plan.

Record GET responses include a strong ETag. PUT/DELETE accept one exact strong
`If-Match`; a mismatch returns 412 without writing. Tags bind database identity,
collection, record ID, last-put revision and a full SHA-256 content digest.
Unrelated writes no longer invalidate them. V4 tags survive a service restart;
legacy tags include a process nonce and do not. Copy-out maintenance changes
identity, so reacquire tags. A returned synchronization/transport error still
does not prove a conditional mutation failed.

Scoped credentials require bootstrap admin authentication, a private admin
directory and v4. Creation requires explicit collection and operation scopes
and a future expiry (at most 365 days). Secret tokens are returned only on
creation/rotation; store them outside URLs, logs, query files and browser
persistence. Rotation supports bounded grace; revocation persists. Grants bind
file lineage and collection identity: switching to a restored/compacted file
or recreating a collection fails closed rather than inheriting old grants.
Credential administration stays bootstrap-admin-only. Scoped diagnostics and
collection listing cannot reveal other collections.

Credential administration can return `credentials_commit_indeterminate` (503)
if replacement succeeded but its directory synchronization failed. The change
is already in effect in memory and on disk but may not survive a crash; no
secret is returned. Re-list credentials and explicitly revoke or reissue as
needed, rather than assuming the operation made no change or blindly retrying.
`credentials_unavailable` instead indicates a failure before replacement.

## Automatic maintenance and backup operations

Automatic maintenance is **off by default**. Admin policy is available at
`/v1/admin/maintenance/auto`; enabling it schedules bounded ticks, skips busy
workers explicitly and uses background rebuilds rather than synchronous query
fallback. Triggers include mutation pressure/stale graph nodes and optional
sampled recall. Recall sampling has a work budget and can be skipped.

Scheduled backups are also off by default and require a private admin directory.
`/v1/admin/backups/schedule` exposes process-local configuration and observed
state; runtime changes do not persist across restart. Each scheduled cycle
creates a consistent snapshot, verifies its manifest and optionally drills a
scratch restored copy. Neither verify nor drill replaces the active owner.

**Retention deletion is separately opt-in.** It only targets generated backups
beyond configured count/age limits, preserving the newest generated backup,
newest verified backup, restores, active/startup-selected files and symlinks.
Do not enable retention without a recovery policy and independent backup copy.
Verification records file integrity; a drill additionally tests reopening and
querying a restored copy. Neither guarantees recovery on untested hardware.

## Operational diagnostics

`/metrics` retains JSON metrics and adds per-collection storage, index, traversal,
buffer, import and operational counters. `/metrics?format=prometheus` provides
text exposition. Counters are process-local, while backup manifests and
credential revocations are persistent. Do not interpret live gauges as a stable
multi-request snapshot.

The bounded slow-query log at `/v1/admin/diagnostics/slow-queries` records mode,
strategy, work counts and duration without vectors, text or filter values.
Persistent administrative audit is exposed separately at
`/v1/admin/audit/persistent`; the old bounded audit ring remains process-local.
Permissions still apply to read-only diagnostics. Backend errors and skipped
maintenance/backup work must be inspected rather than treated as success.
