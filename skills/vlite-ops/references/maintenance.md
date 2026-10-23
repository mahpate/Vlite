# Maintenance, encryption and errors

## Actual CLI grammar

Build with `cargo build --bin vlite-maintain`. Positional grammar in
`src/bin/vlite-maintain.rs`:

```text
vlite-maintain inspect SOURCE
vlite-maintain backup SOURCE DESTINATION
vlite-maintain restore BACKUP DESTINATION
vlite-maintain compact SOURCE DESTINATION
vlite-maintain upgrade SOURCE DESTINATION
vlite-maintain verify PATH
vlite-maintain drill BACKUP WORKDIR
```

No `--db`, `--key`, `repair`, `vacuum` or in-place compaction option exists here.
Stop owners first. Inspect validates the whole source, reports format,
dimensions, records, commits, file/valid/tail bytes and exits nonzero for an
incomplete suffix. Verify reports inspection plus bytes/CRC. Neither repairs.
Backup/restore reject corrupt or incomplete files. Preserve a closed byte copy
through your filesystem/forensic process before recovery instead of assuming
the backup CLI accepts damaged input.

Copy-out commands never clobber a destination. `drill` validates a backup via a
restore and probes in an existing work directory. Create a new empty directory
first (it does not create WORKDIR for you); do not point it at existing data.
Compaction removes obsolete log entries and changes file-local sequence/identity
assumptions. Reacquire revisions/ETags, restart change-feed consumers correctly
and rebuild sidecars. Verify installed format semantics; `upgrade` is not a
"latest everything" switch.

## Encryption and repair

Use `vlite::maintenance::{inspect_with_key, verify_with_key, backup_with_key,
restore_with_key, compact_with_key}` for encrypted files. Keyless CLI calls fail
with `EncryptionKeyRequired`; never decrypt just to work around that.
`maintenance::reencrypt(src, dest, old_key, new_key)` rotates to a new validated
file. Backups stay encrypted. Coordinate key storage with backup retention.
See repository `docs/STORAGE-FEATURES.md` for signatures/caveats.

`Database::discard_incomplete_tail(path)` (or
`discard_incomplete_tail_encrypted(path, &key)`) is an explicit destructive Rust
API, not a CLI command. Preserve the closed original byte-for-byte first. Only
a structurally incomplete final frame is discardable; a complete bad checksum/
header/sequence remains `Corrupt`. A truncated acknowledged commit cannot be
distinguished from an interrupted unacknowledged write; repair may lose
acknowledged data. Never truncate arbitrary bytes by hand or invoke repair
automatically. Inspect, reopen and verify the retained prefix afterward.

## Error triage

These are Rust `Error` variants in `src/lib.rs`. Wire protocols commonly use
lowercase snake-case codes (Python `EmbeddedError.code` includes `locked` and
`read_only`). Check the active protocol; capitalization is not shared verbatim.

- **Locked**: exclusive owner or shared readers exist. Close legitimate owners;
  do not delete locks/files or kill unrelated processes.
- **ReadOnly / ReadOnlyReplica**: mutation refused. Reopen deliberately writable
  only after authorization/all readers close; followers need documented promotion.
- **Corrupt**: preserve, stop use, investigate storage and recover a verified
  backup. Tail repair cannot discard complete corrupt frames.
- **IncompleteTail**: open refuses incomplete suffixes. Preserve the original
  before any authorized destructive tail repair.
- **CommitIndeterminate / Poisoned**: writes/sync may have reached disk. Close,
  inspect/reopen, reconcile IDs/revisions, then decide whether to retry. No
  exactly-once retry guarantee is implied.
- **RepairIndeterminate / PublicationIndeterminate**: inspect source/reported
  destination before retry; a complete published output may already exist.
- **DimensionMismatch / InvalidDimensions / NonFiniteVector / ZeroNormQuery**:
  validate model, vector length/finiteness; cosine needs nonzero query norm.
  Do not silently pad/truncate vectors to bypass validation.
- **IndexNotReady / UnsupportedAnnMetric / InvalidEfSearch**: align graph,
  metric/ef; use exact or explicit build. HNSW does not support inner product.
- **EncryptionKeyRequired / EncryptionKeyMismatch / NotEncrypted**: retrieve
  correct credentials from secret storage; never log keys or reset data.
- **SidecarUnavailable / StaleIndex**: rebuild a permitted cache for the current
  state; encrypted files refuse plaintext ANN sidecars.
- **UnsupportedVersion / UnsupportedFeature / CollectionsRequireUpgrade**:
  compatible binary or explicit copy-out migration, never header edits.
- **RevisionMismatch**: reread/resolve the application conflict; do not silently
  replace conditional operations with unconditional ones.

Reopen/CRC success is not proof against every hardware fault. Keep external
backup/history evidence and test application-level recovery.
