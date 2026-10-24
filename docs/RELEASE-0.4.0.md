# VLite 0.4.0 upgrade and rollback

This release packages the concurrency, binding, dashboard and stability work
since 0.3.0. It remains experimental: the live dataset resides in memory,
startup replays the log, and one exclusive writer or multiple shared-lock
read-only owners may open a file. No storage format version changes.

## Highlights

- Shared-lock read-only Rust, Python and Node opens; concurrent server reads
  and parallel native exact scans.
- Bounded committed Rust snapshots for exact search, get and browse.
  Simple server exact searches scan outside the owner lock by default;
  `--lock-held-reads` selects lock-held execution when generation copying is
  a worse workload trade-off.
- Opt-in durable server group commit shares syncs among bounded concurrent
  writes without weakening sync-before-acknowledgement or merging their atomic
  recovery frames. It is disabled by default.
- Typed float32 C/Python inputs and copied Float32Array Node search, atomic
  bulk put and multi-query APIs; existing JSON methods remain available.
- Reduced HNSW selection allocations, compatible CRC/replay improvements and
  shared unquantized background rebuild generations.
- Snapshot-retention and group admission/queue/sync metrics, rebuild phases
  and opt-in per-collection scheduled retry backoff.
- Dashboard and operational tooling improvements, plus crash/short-write/
  failed-sync and seeded mixed-load regression coverage.

See [snapshots](read-snapshots.md), [operations](CONSOLE-ADMIN.md),
[Python](../bindings/python/README.md), [Node](../bindings/node/README.md)
and [measurement limitations](../benchmarks/README.md). Historical 0.3.0
benchmark results remain labeled 0.3.0 and are not new release measurements.
Unfinished property-graph development is not included in this release.

## Upgrade

Stop writers, retain an independent verified backup, and test a copy with the
new binaries before replacing a running service. Download matching artifacts
and verify the release's SHA256SUMS and release manifest. GitHub assets,
container publication and PyPI publication have separate verification stages;
do not assume a registry upload exists merely because a tag exists.

Existing V3/V4/V5 files remain supported; no automatic migration is needed.
Snapshot generations retain superseded record bodies and the first later write
can copy the ID tree. Logical capture budgets are not resident-memory limits.
Compare reader/writer tail latency on your workload before enabling group
commit or changing the read policy. Do not automatically retry a
`commit_indeterminate` response.

## Rollback and limits

Stop the new service before restarting old binaries against the same file.
Preserve backups and any explicit ANN sidecars; derived indexes can be rebuilt.
Keep creation flags and file features compatible with the old version and
restore a verified copy when in doubt. New client methods require the matching
native library; old libraries may not export their symbols.

The release workflow runs native Linux, Windows and macOS checks and builds
Linux arm64 command-line assets and multi-platform images. Available artifact
platforms are listed in the release manifest, not implied by source portability.
Process interruption and injected I/O failures do not qualify physical
power-loss behavior. Windows directory-entry durability retains its documented
limitations. This release adds neither distributed transactions nor a
production-hardening guarantee.
