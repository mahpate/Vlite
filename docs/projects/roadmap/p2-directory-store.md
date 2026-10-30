# Speed P2: directory store

| Field | Value |
|---|---|
| Status | P2a in progress: the checkpoint image format has landed; the directory layout, checkpoint and converters are next |
| Goals | F4 (reopen time), S6 (no unbounded log growth), no regressions ([`storage-goals.md`](../performance/storage-goals.md)) |
| Roadmap | [`next-major.md`](next-major.md) |

## Problem

A `.vlite` file is one append-only log. Reopening replays every frame since
creation, and the file only shrinks through offline compaction. Compaction
gives the file a new lineage and renumbers every revision. That breaks
change-feed cursors, replicas and compare-and-set or fencing tokens held by
callers. A long-running server therefore pays for its whole history on every
restart, or it loses its identity to get rid of that history.

## How other stores solve it

| Store | Pattern | What we take |
|---|---|---|
| RocksDB, LevelDB | Write-ahead log plus immutable files, listed in a `MANIFEST`; a one-line `CURRENT` file names the live manifest and is swapped atomically | `CURRENT` and `MANIFEST-N` with an atomic rename, and ignoring files that the manifest does not list |
| Qdrant, Milvus, Lance | Segments sealed when full and merged in the background; the WAL is truncated once its contents are in a segment | WAL rotation, background merge, size threshold |
| SQLite WAL, Postgres | Checkpoints fold the WAL into the main image while readers keep their snapshot | A checkpoint is a background step, not a maintenance command |
| etcd, Raft logs | A snapshot replaces the log prefix, the log keeps its index, and requests for compacted history return an error | Keep sequences across a checkpoint, and return an explicit error for cursors below the floor (`cursor_before_history`, like etcd's `ErrCompacted`) |

## Design

### Directory layout

```text
db.vlite.d/
  LOCK            exclusive or shared OS lock, held while open
  CURRENT         names the live manifest; replaced by temp, fsync, rename, fsync directory
  MANIFEST-N      ordered file list and history floor, protected by a CRC
  image-N.vlite   checkpoint image (FLAG_HISTORY), optional
  wal-N.log       write-ahead logs, oldest first; only the last one is appended
```

Each file starts with a full v5 header. Its magic, version, dimensions,
lineage, cipher descriptor and graph flag must match the other files. The
engine reads the chain as one logical log, so the existing replay, change
feed, log segments and graph code run unchanged. All files share one cipher
descriptor. A file that the manifest does not list is ignored on open and
removed by the next checkpoint.

### Checkpoint image (landed)

A checkpoint image is an ordinary v5 file with `FLAG_HISTORY`:

1. An advance frame (op 13) at the floor `S`, the last sequence the image
   covers.
2. Collection creates and settings at sequence 1.
3. Each live record at its original revision, ascending.
4. Vector data, open import-job markers and graph state at `S`.

The loader accepts sequences that do not decrease and stay at or below `S`
while restating. Strict contiguity resumes at `S + 1`. These are rejected as
corrupt:

- an advance frame anywhere except first;
- a history image without an advance frame;
- a sequence that decreases, or skips past `S + 1`;
- history combined with commit timestamps.

Compaction drops the flag and renumbers from one, as before.

### Why revisions survive

A revision is the sequence of the commit that last wrote a record. The image
writes each live record at that sequence. Replay therefore assigns the same
revisions, and conditional writes and fencing tokens behave exactly as they
would without the checkpoint. Deleted records have no revision to keep. A later
put still gets a sequence above `S`, so revisions only grow and are never
reused. Graph entities restate at `S`, which is what compaction already does
for them.

What changes is history, not state:

- Change-feed and log-segment requests from a cursor below `S` return 409
  `cursor_before_history`. A consumer takes a snapshot and resumes from a
  cursor at or above `S`.
- A new replica bootstraps from a backup, not from sequence 0.
- Point-in-time restore cannot target a sequence below `S`.

### Checkpoint steps

1. Stop the flusher and sync.
2. Rotate: open `wal-(N+1)` and publish a manifest that lists it. New commits
   go there.
3. Load the sealed prefix and require a complete load whose sequence equals
   `S`.
4. Write the image to a temporary file, validate it, fsync it and rename it.
5. Publish a manifest of `[image, wal-(N+1)]`, then swap the chain's prefix
   in memory.
6. Delete the files that the manifest no longer lists, and restart the
   flusher.

A crash at any step leaves either the old manifest or the new one. Both
describe the same committed state. Partial images and orphaned WAL files are
not listed, so they are ignored.

### Converters and limits

- File to directory: a checkpoint image at the file's sequence, plus an empty
  WAL. The lineage, revisions and cursors are kept.
- Directory to file: concatenate the chain, skipping every header after the
  first. The result is an ordinary `FLAG_HISTORY` file.
- Directories do not support commit timestamps, in-memory `to_bytes`, or the
  offline file tools such as compaction and repair. These return explicit
  errors; pack the directory to a file first.

## Staging

| Stage | Contents | Exit |
|---|---|---|
| P2a | Checkpoint image (done), directory layout, `create_directory`, checkpoint, background merge at a WAL threshold (64 MiB default), converters | S6. Reopen replays one image plus a bounded WAL. Revisions, cursors at or above the floor, and lineage survive checkpoints |
| P2b | Memory-mapped immutable segments; the index and graph are loaded from the image instead of replayed | F4 (reopen in 50 ms or less) |

P3 to P5 build on these segments: compact vectors, CSR graph adjacency, and
per-segment statistics.
