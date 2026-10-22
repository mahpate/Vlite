# VLite 0.2.0 upgrade and rollback

This experimental release adds opt-in v4 collections, durable record revisions,
metadata query planning, hybrid retrieval, checkpointed imports, scalar-u8 ANN,
operational diagnostics, scoped credentials and optional maintenance/backup
scheduling. See [capability contracts](CAPABILITIES.md) for limits and
[Docker operations](DOCKER-OPERATIONS.md) for deployment commands.

Optional [ANN snapshots](ANN-SNAPSHOTS.md) preserve validated derived graph
state across graceful restarts. The optional [gRPC transport](GRPC.md) adds a
bounded protobuf data API over the same database owner and authorization.
Both are disabled by default.

## Compatibility

Existing v1-v3 files remain readable and are not silently upgraded.
`Database::create` and server `--create-if-missing` still create v3 by default.
To create v4, explicitly use the format-aware core API or server
`--create-format 4`. Older binaries cannot open v4.

The Rust error enum is non-exhaustive. Downstream exhaustive matches must
include a wildcard. V4 ETags survive restart; legacy-file ETags remain
process-local. Conditional writes now invalidate only the changed record's
tag. Copy-out compaction, upgrade and restore change lineage, invalidating tags
and scoped grants: reacquire tags and reissue grants after selecting a new copy.

Schedulers and retention deletion are disabled by default. Runtime scheduler
configuration is process-local. The operator must secure the admin directory;
the service does not provision OS-level access policy.

Scalar-u8 traversal still retains original f32 vectors for exact reranking.
Component bytes are not process RSS, and total RAM savings are not guaranteed.
Indexes remain derived data, not an alternative source of committed records.

## Safe upgrade sequence

1. Stop application writes or arrange a maintenance window. Create a consistent
   administrative backup and retain an independent copy outside the volume.
2. Record the currently running API/dashboard image digests. Preserve those
   images before replacing their local tags.
3. Deploy matching API/dashboard binaries while leaving the existing v3 volume
   and database selection unchanged. Check health, authenticated capabilities,
   existing record counts and a read-only exact query.
4. Trial v4 offline on a backup, never on the active file:

   ```text
   vlite-maintain upgrade backup.vlite trial-v4.vlite
   vlite-maintain verify trial-v4.vlite
   vlite-maintain drill trial-v4.vlite EXISTING_SCRATCH_DIRECTORY
   ```

5. Exercise collections, import replay/resume and application retrieval against
   the separate trial. Only an explicit operator decision should select a new
   production copy. Keep the original v3 recovery copy.

Do not copy an actively written database with a generic filesystem command:
use the serialized backup operation or stop its owner first.

## Rollback

Stop new writes and preserve the current database/admin state before rollback.
Recreate only VLite's API and dashboard with the recorded compatible image
digests and the same volumes. Do not remove volumes or use `compose down -v`.
Do not alter other applications running on the host.

If the live file stayed v3, the earlier compatible binary can reopen it.
If a v4 copy was selected, do not point a v3-only binary at it. Restore a
validated pre-upgrade copy to a separate path and explicitly select that path
offline. A rollback of binaries alone does not roll back committed data.

## Release verification

Verify downloaded payloads against `SHA256SUMS` and inspect
`release-manifest.json` for their source commit. Checksums detect corruption;
they are not signatures. Use matching API/dashboard release versions or
immutable image digests. Release automation validates package-version identity,
native binaries, installed SDKs, container manifests, HTTPS and browser behavior
before publishing the GitHub release. Packages are not automatically published
to npm or PyPI.
