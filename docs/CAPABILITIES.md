# Collections and advanced retrieval

These APIs extend the embedded engine. Plain `Database` methods use the
`default` collection, and files are never upgraded automatically.

## File compatibility and collections

`Database::create` makes a v3 file. Create v4 for named collections and
durable import checkpoints:

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

`cargo run --locked --example collections -- new-example.vlite` runs an
end-to-end example of this page's features. It refuses an existing output
file.

Each collection has its own dimensions, record IDs, settings and derived
indexes. `default` cannot be dropped; a named collection can be dropped only
when it is empty and has no open import job. Invalid settings and exceeded
limits fail without creating anything.

Older binaries cannot open v4 files. Stop the file's owner, then upgrade to a
**new, nonexistent path** and keep the original:

```powershell
cargo run --bin vlite-maintain -- upgrade old.vlite new.vlite
cargo run --bin vlite-maintain -- verify new.vlite
```

Upgrading a copy does not switch a running service; verify the new file, then
select it. v4 restore and compaction assign a new file identity, and derived
indexes are rebuilt rather than stored.

Collection IDs are never reused within a v4 lineage, including across
copy-out maintenance. When the ID space runs out, creating a collection fails
without writing, even below the live-collection limit. A closed import keeps
its collection name and a dropped flag, so recreating the name cannot resume
it.

`Error` is `#[non_exhaustive]`; match it with a wildcard arm.

## Opt-in v5 record features

Create a `FormatVersion::V5` file for record expiry, GeoPoint metadata and
metadata-only updates. Opening never migrates a file, v1–v4 files behave as
before, and older binaries reject v5.

- `put_with_expiry(id, vector, payload, metadata, expires_at_ms)` sets an
  absolute Unix-millisecond expiry. A record is expired once the query clock
  reaches it. `get_at` and `search_at` take an explicit clock; other reads
  use the system clock.
- `maintenance::compact_at(source, destination, now_ms)` leaves expired
  records out of the new copy, keeps the other expiries and never modifies the
  source.
- `update_metadata(id, &metadata)` rewrites only the metadata, keeps the
  vector, payload and expiry, and advances the revision.
- On older formats, expiry and metadata-only operations return
  `ExpiryRequiresUpgrade`.
- `search_grouped` returns nearest-first groups with a per-group hit cap.
  `recommend(&RecommendationQuery)` searches with the mean of the positive
  examples minus the mean of the negative ones and excludes the examples.
  `facets` and `numeric_stats` summarize metadata.
- `MetadataValue::GeoPoint` stores bounded latitude and longitude.
  `Condition::GeoRadius` filters by Haversine radius, and
  `search_geo(&GeoSearchQuery)` ranks by vector or geographic distance.

These query-time operations need no persistent indexes.

## Record revisions

`revision(id)` returns the sequence of the record's last successful put.
Revisions are per collection and record, and records in one atomic batch can
share a revision, so never compare bare revisions across files, collections
or IDs. `Expected::Absent` and `Expected::Revision(revision)` make puts and
deletes conditional; a mismatch writes nothing. The check and the write use
the same exclusively owned handle.

## Metadata indexes and automatic planning

Declare indexes in `CollectionSettings::metadata_indexes` with
`MetadataIndexSpec` and `MetadataIndexKind::{Eq, Range, EqRange}`. They update
after each successful commit and are rebuilt on reopen or settings
replacement. Candidates are always rechecked against the full typed filter,
and indexed exact results match an unindexed scan: same IDs, distances and
order.

`CollectionRef::search_auto_with_settings(query, k, conditions, ef_search)`
returns hits, the chosen `QueryPlan` and `SearchStats`. It chooses
`exact_scan`, `exact_indexed`, `ann` or `filtered_ann`: selective indexed
queries score candidates exactly, broad queries can use a ready ANN index, and
missing indexes mean exact search. **Auto mode never builds an ANN index.**

Settings persist in v4. On a legacy default collection they are memory-only,
reported as `SettingsPersistence::MemoryOnly`.

## Text and hybrid search

Index up to eight string metadata fields with
`CollectionSettings::text_fields`; payloads are not indexed. Text is split on
non-alphanumeric characters and lowercased, with no stemming, phrase search or
language-specific analysis. Token, query and posting limits apply. If the
posting limit disables the index, writes still succeed and text queries return
a readiness error.

`HybridQuery::new(Some(vector), text, k)` fuses vector and BM25 results with
weighted reciprocal rank fusion, applying the same metadata filter to both.
Fusion scores are not distances: **higher is better**, and ties break by ID.
Hits include each leg's rank, the vector distance if any, and the BM25 score.
The default vector leg is exact; `VectorLeg::ApproximateReady` never builds an
index and can miss candidates. Candidate depth and weights change results.

## Quantized ANN, memory and quality

Pass `AnnOptions { quantization: Quantization::ScalarU8, rerank_multiplier: 4 }`
to the `*_with_options` APIs. Traversal uses scalar-u8 codes, then reranks
candidates with exact distances from the current f32 vectors; buffered writes
and metadata filters also use current records. Quantization is part of the
index identity, so legacy APIs never use a quantized graph.

`ann_memory` reports component bytes for graph links, codes and codebook,
buffer and resident vectors. These are **not measured process RSS**. The f32
vectors stay resident, so quantization can add code storage and does not
promise lower total memory. `ann_quality` samples a ready index against exact
search within a budget and reports a reason when it skips. Recall depends on
the workload and is not guaranteed. The
[benchmark guide](../benchmarks/README.md#indexed-and-quantized-search-comparison)
reproduces a bounded comparison.

## Resumable ingestion

On v4, `import_chunk(job, chunk, operations, policy)` atomically commits
accepted rows and a durable checkpoint in one synchronized frame. Legacy files
return `ImportsRequireUpgrade`.

- **Replay.** Replaying the latest chunk with the same SHA-256 identity
  returns `AlreadyCommitted` without writing. Different content or policy, a
  sequence gap or the wrong collection is a conflict. Job IDs increase
  monotonically; `next_import_job_id` allocates the next one.
- **Policies.** `RejectChunk` reports invalid rows and writes nothing.
  `SkipInvalid` commits valid rows and reports skips with bounded details; an
  all-skipped chunk still records its checkpoint.
- **Closing.** `finish_import` and `abort_import` close a job. **Abort does
  not roll back committed chunks.**
- **Bounds.** A chunk, including checkpoint overhead, must fit the
  4,096-operation / 16 MiB batch limits. Counters are full-width integers.
  Compaction keeps open-job counters and the latest replay identity but may
  drop closed-job status, so the job registry is not an audit log.
- **REST reservations** stay in memory until their first chunk commits, and
  first chunks must follow reservation order. While an earlier reservation on
  a live collection is uncommitted, a later job gets `import_conflict` and
  nothing is written; commit or abort the earlier one, then retry.
- **Failures.** A failed write can leave an incomplete trailing frame with an
  unknown outcome, and the handle fails closed. Repair the tail, reopen,
  inspect the checkpoint, then replay. A transport or sync error does not
  prove the chunk was not committed.

## REST service and scoped credentials

`vlite-server ... --create-if-missing --create-format 4` creates a v4
database; the default is v3, and existing files are never migrated.
`GET /v1/capabilities` reports the format, role and supported flags.

- Collection routes live under `/v1/collections/{name}` and never fall back
  to `default`. Legacy vector and search routes are `default` aliases with
  their own hard defaults; collection routes use collection settings.
- REST settings updates merge fields; the embedded setter replaces all
  settings.
- u64 record IDs, seeds and revisions are decimal strings.
- Search `explain` adds work counters and, in auto mode, the chosen plan.
- Record GETs return a strong ETag for conditional PUT and DELETE; see
  [conditional updates](REFERENCE.md#conditional-updates-and-deletes-optimistic-concurrency).
- **Scoped credentials** (API keys) need super admin authentication (the
  admin token or a super admin account), a private admin directory and v4.
  Each needs explicit collection and operation scopes and an expiry at most
  365 days ahead. Secrets appear only on creation or rotation; keep them out
  of URLs, logs, query files and browser storage. Rotation allows a bounded
  grace period, and revocation persists. Grants are bound to file lineage and
  collection identity, so restored or compacted files and recreated
  collections fail closed. Scoped diagnostics and listings never reveal other
  collections.
- Capabilities include a display-only
  [`identity`](CONSOLE-ADMIN.md#configuration-and-roles), and
  `features.tenant_lifecycle` when tenant management is available; see
  [SECURITY.md](SECURITY.md#tenants).
- `credentials_commit_indeterminate` (503) means the change is in effect in
  memory and on disk, but its directory sync failed, so it may not survive a
  crash; no secret is returned. Re-list credentials and revoke or reissue as
  needed instead of retrying blindly. `credentials_unavailable` means the
  failure came before the change.

## Automatic maintenance and backups

Both are **off by default**.

- **Maintenance** (`/v1/admin/maintenance/auto`) runs bounded ticks, skips
  busy workers and uses background rebuilds rather than a synchronous query
  fallback. Triggers are mutation pressure, stale graph nodes and optional
  sampled recall, which has a work budget and can be skipped.
- **Scheduled backups** need a private admin directory.
  `/v1/admin/backups/schedule` shows process-local configuration and state;
  runtime changes are lost on restart. Each cycle takes a consistent
  snapshot, verifies its manifest and can drill a restored scratch copy.
  Verify proves file integrity; a drill also reopens and queries the copy.
  Neither replaces the active database or guarantees recovery on untested
  hardware.
- **Retention deletion is a separate opt-in.** It removes only generated
  backups beyond the count or age limit and always keeps the newest generated
  backup, the newest verified backup, restores, active or startup-selected
  files and symlinks. Enable it only with a recovery policy and an independent
  backup copy.

## Operational diagnostics

- `/metrics` (JSON, or Prometheus text with `?format=prometheus`) adds
  per-collection storage, index, traversal, buffer, import and operational
  counters. Counters are process-local; see
  [logs and metrics](REFERENCE.md#logs-and-metrics).
- `/v1/admin/diagnostics/slow-queries` is a bounded log of mode, strategy,
  work counts and duration, without vectors, text or filter values.
- `/v1/admin/audit/persistent` holds the persistent admin audit; the bounded
  audit ring is process-local.

Read-only diagnostics still check permissions. Inspect backend errors and
skipped maintenance or backup work; do not treat them as success.

## Embedded vector extensions

v5 also supports named and multi-vector records, bounded per-model embedding
history, sparse inverted-index dot-product search, dense/sparse RRF, and
resumable, revision-checked re-embedding with an atomic switch of the active
version. Provider and reranker traits and a read-only query language are Rust
APIs. See [VECTORS.md](VECTORS.md) for APIs, grammar, limits and examples.

## Property graphs (experimental, source-only)

The non-default `graph` build feature stores named property graphs in
graph-enabled v5 files that older and non-graph builds refuse. It is in no
release artifact. See [GRAPH.md](GRAPH.md).
