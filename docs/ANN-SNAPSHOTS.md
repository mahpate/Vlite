# Optional ANN snapshots

The primary VLite database file is always the source of committed records.
An ANN snapshot is an optional derived cache, not a backup, journal or recovery
mechanism. Losing it must never lose a committed record.

Start the server with `--ann-snapshot` to load `<active-database-path>.ann`
after the database opens and save the ready in-memory index state after
graceful listener shutdown. Without that flag, the server neither reads nor
writes this sidecar. Keep adequate graceful-stop time and free disk space;
large snapshots can take time to validate and write.

Embedded callers opt in explicitly. Rust uses
`Database::save_ann_snapshot(path)` and `Database::load_ann_snapshot(path)`
with `Database::ann_snapshot_path(datafile)` (`<datafile>.ann`) as the
conventional path. The C ABI exposes the same pair as `features` operations
`save_ann_snapshot` (`path`; returns `{indexes,bytes}`) and `load_ann_snapshot`
(`path`; returns `{status,indexes}`), and the Python package as
`Database.save_ann_snapshot(path)` / `Database.load_ann_snapshot(path)`. A
`status` other than `loaded` (`missing` or a rejection code such as a stale
identity after later commits) installs nothing; call `build_index` as before.
Encrypted databases refuse to save. `benchmarks/compare_zvec.py
--vlite-ann-snapshot` measures this path: it saves after the build and loads on
reopen instead of rebuilding.

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

The handle-local `set_background_hnsw_rebuild(Some(limits))` option does not
change the snapshot format. Its growing mutation delta can exceed snapshot
v1's 256-ID / 8-MiB limits; saving then returns an explicit error. Publish a
new rebuild before saving. Existing snapshots remain readable, and loading
one preserves the handle's background-rebuild option.

Snapshot loading avoids graph reconstruction, not database replay or validation.
Report measured restart time rather than promising instant startup. Original
f32 records remain resident even with scalar-u8 traversal; persisting the graph
does not establish total RAM savings.
