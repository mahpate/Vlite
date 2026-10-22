# Optional ANN snapshots

The primary VLite database file is always the source of committed records.
An ANN snapshot is an optional derived cache, not a backup, journal or recovery
mechanism. Losing it must never lose a committed record.

Start the server with `--ann-snapshot` to load `<active-database-path>.ann`
after the database opens and save the ready in-memory index state after
graceful listener shutdown. Without that flag, the server neither reads nor
writes this sidecar. Keep adequate graceful-stop time and free disk space;
large snapshots can take time to validate and write.

The cache identity includes the database format, lineage, committed sequence
and offset, collection identity and live-vector/revision digests. It also
carries a versioned format and integrity checksum. A stale, corrupt,
unsupported, oversized or unreadable cache is rejected explicitly rather than
partially installed. Rebuild using the existing index controls; loading a bad
cache never repairs or rewrites the primary database.

An abrupt exit can leave a missing or older snapshot. Any later database
commit makes an earlier snapshot stale; this release does not replay log
changes into a saved graph. Copy-out maintenance changes identity, and
maintenance tools do not copy sidecars. A restore selection uses the selected
file's own sidecar path.

The sidecar contains sensitive index/vector information. Apply the same local
access policy as the datafile, and exclude it from public attachments and source
control. Preserve the primary-file lock: never share the same sidecar between
independent database owners. The cache is replaceable; an older binary that
does not support snapshots ignores it.

Snapshot loading avoids graph reconstruction, not database replay or validation.
Report measured restart time rather than promising instant startup. Original
f32 records remain resident even with scalar-u8 traversal; persisting the graph
does not establish total RAM savings.
