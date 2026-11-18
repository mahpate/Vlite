# Task C — HA EPIC-006 design report: writer authority, epochs, fencing (report only)

Repository: mahpate/Vlite (Rust). You work in a fresh clone at the plan base.
This is a **design/investigation task**. Do not change any source, test or doc file.

## Output (the only file you may create)

`pool-briefs/reports/C-epic006-design.md`

## Read first

- `docs/projects/feature-aca-ha/feature-aca-ha.prd.md`: section 3.8 (~243),
  section 3.11 (~268), FR rows on writer authority/epochs, OD-002 and MEAS-006,
  ITEM-023 (~853), ITEM-024 (~854), the failure modes (FM-*) and acceptance
  criteria (AC-*) they cite, and the decisions log / change log at the end.
- `src/cloud_backup.rs` (archive publication, head/manifest objects, conditional
  writes / ETags, metadata), `src/server/cloud_recovery.rs` (`connect`, Staging lock,
  `Sets` list/pin/unpin/release/fetch_with), `src/archive.rs` (`follow_archive`,
  `Follow`), `src/server/serve_mode.rs` (`admit`, `status`, `"epoch": null`),
  `src/server/gateway.rs` and `src/server/gateway_mode.rs` (how the gateway picks
  and trusts backends; `x-vlite-*` header pass-through), `src/server/reader_apply.rs`,
  `src/bin/vlite-server.rs` (serve-mode flags and startup refusals),
  `docs/API.md` serve-status and gateway sections, `docs/SECURITY.md`.

## Design the report must cover (concrete, code-level)

1. **Authority object**: name, location (beside the archive head), JSON schema
   `{writer_id, epoch E, …}`, how it is created/claimed, and the exact conditional
   write used (If-Match / If-None-Match ETag CAS with the existing storage client in
   `cloud_backup.rs`; say which function to extend and whether every supported backend
   — local directory store and Azure Blob — offers the needed precondition; if a
   backend cannot, say how it is refused).
2. **Epoch in archive metadata**: where E is stamped on each published batch/head,
   and how readers (`follow_archive`, reader_apply) reject a batch whose epoch is
   below the authority's current epoch.
3. **Head If-Match CAS**: how a writer publishes the head conditionally so a fenced
   (old-epoch) writer's publish fails, and what it does on that failure.
4. **Startup and serve checks**: writer startup claim/verify sequence, periodic
   re-verification, and what serve-status reports (`"epoch"` replacing `null`).
5. **Per-batch verification** (OD-002 / MEAS-006): the check each batch performs and
   its measured or estimated cost; propose how MEAS-006 is measured without a new
   benchmark campaign.
6. **Fenced self-stop**: the exact state machine when a writer discovers a newer
   epoch (stop accepting writes → drain → refuse readiness → exit code / log), and
   how it interacts with `draining()` and the gateway.
7. **Epoch response stamp**: a header (e.g. `X-VLite-Epoch`) on writer and reader
   responses and how the gateway's `check_epoch` uses it; must be absent when HA is
   off (AC-010: HA-off behaviour byte-identical).
8. **File/function plan**: new `src/server/authority.rs` (types, claim, verify,
   CAS helpers), changes per file (`cloud_backup.rs`, `archive.rs`,
   `serve_mode.rs`, `server.rs`, `gateway.rs`, `vlite-server.rs`), new CLI flags if
   any, and an ordering that lets two or three engineers work in parallel with
   disjoint write scopes.
9. **Test list**: unit and integration tests (names + one-line assertions) covering
   claim, double-claim race, fenced publish, reader rejection of stale-epoch batches,
   self-stop, header presence/absence, HA-off unchanged.
10. **Open questions / risks**, each with a recommended default so implementation can
    proceed without the owner. Constraints: **no new crate dependencies**, no new
    cloud services (no Key Vault; reuse the existing archive storage and ACA app
    secrets), never touch `deploy/`.

Cite file paths with line numbers for every existing function you rely on. Keep the
report under ~600 lines.

## Validation

None beyond confirming that `git status` shows only the report file added. Commit
the report on your result branch.
