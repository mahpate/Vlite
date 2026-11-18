# EPIC-006 design: writer authority, epochs and fencing

**Disposition: design investigation complete; fencing implementation and runtime validation NOT RUN.**
Inspected revision: `7ce3dc51c521388ebbbd5c2cc05c62315500d4c9`.
This report proposes implementation, not a passed HA gate, measured performance result,
security audit, deployment approval or production-readiness claim.

The outer assignment's report location and prohibition on commits override the
different output path and commit instruction in `pool-briefs/C.md:7-9,69`.
No source, deployment configuration or other report was changed.

## 1. Findings and controlling decisions

1. **Existing index CAS is not writer fencing.** The current mutable head is
   `sets/{set_id}/archive.json`, not a tenant-wide head. `Sets::archive_key`
   and `open_archive` are at `src/server/cloud_backup/archive.rs:206-263`;
   `append_checkpoint` checks a callback and CASes this baseline-specific
   object at `:273-393`. Changing another object's epoch leaves that ETag
   unchanged. A zombie can pass its check, pause, and later successfully CAS
   the old index. Separate authority and head writes are not transactional.
2. **Use one stable authority/head CAS serialization point.** Keep immutable
   batch/index objects beside the existing archive objects, but make one stable
   prefix-level object contain both writer identity/epoch and the authoritative
   head pointer. An epoch bump then invalidates every earlier head CAS token.
   Do not claim that a per-epoch head alone fences a former epoch.
3. **No production local-directory object-store adapter exists here.**
   `ObjectStore` is the trait at `src/server/cloud_backup.rs:164-172`;
   implementations found in `src/` are Azure `Container`
   (`src/server/cloud_backup/azure.rs:574-704`) and test-only `Memory`
   (`src/server/cloud_backup.rs:1913-1916,1960-2119`).
   `cloud_recovery::connect` unconditionally uses Azure
   (`src/server/cloud_recovery.rs:239-246`).
   The database directory store is different: `directory::open` takes a
   filesystem `LOCK`, not object ETag CAS (`src/directory.rs:858-867`).
   A local file lock is not cross-machine writer authority.
4. **Serve-time fencing is absent.** `serve_mode::admit` currently only rejects
   reader mutations (`src/server/serve_mode.rs:111-125`), and `status` emits
   `"epoch": null` (`:142-170`). `gateway::check_epoch` exists but is unused;
   the actual forwarding path does not call it
   (`src/server/gateway.rs:545-559`; `src/server/gateway_mode.rs:345-498`).
5. **Reader integration is not complete at this revision.** Archive follow and
   generation-swap primitives exist, but the inspected `src/` call sites of
   `follow_archive` and `swap_reader_generation` are tests only
   (`src/server/cloud_recovery.rs:2605-2608`;
   `src/server/reader_apply.rs:215-275`). Do not treat PRD decisions about a
   reader enrollment loop as delivered code.
6. **Important startup incompatibility:** gateway-trusted backends currently
   refuse readers and `VLITE_ADMIN_DIR`
   (`src/bin/vlite-server.rs:532-550`), while archive scheduling/capture is in
   the admin cloud runtime. Simply adding an authority poller will not wire
   a functional trusted HA writer or reader.

Controlling requirements: PRD sections 3.8-3.11 (`:243-276`), FR-001/006/013/014,
NFR-001/004 and CON-001/004/PAT-001 (`:300-332`), Q-1..6 (`:369-378`),
MEAS-006 (`:410`), AC-007/009/010 (`:423-426`), FM-001/002/003/005/006
(`:436-441`), and ITEM-023..026 (`:848-856`).
All PRD references in this report mean
`docs/projects/feature-aca-ha/feature-aca-ha.prd.md`.
The 2026-11-18 OD-002 decision requires checking epoch **and writer ID before each write-batch acknowledgement**, not just before asynchronous archive upload (PRD `:496`, reinforced at `:513-522`). Older notes calling OD-002 open are history.
Latest decisions preserve ACA app-secret key delivery, no new dependencies or services, ZRS/no geo failover, manual promotion and unpassed production gates (PRD `:503-522,923-947`).

## 2. Concrete authority and publication protocol

### Object layout and schema

One authority scope is one writable database/archive prefix. Dedicated tenants
get distinct prefixes. Tenants sharing a single database cannot independently
promote its physical log; `shared-pool` stays authority-off in this slice.
Use the existing target's **coordination container** and configured prefix:

```text
{prefix}authority.json                       stable combined authority + HA head
{prefix}sets/{set_id}/manifest.json           existing immutable baseline manifest
{prefix}sets/{set_id}/objects/{sha256}         existing immutable payload objects
{prefix}sets/{set_id}/ha-index/{sha256}.json   immutable epoch-bound Index revision
```

`authority.json` is the logical HA archive head, adjacent to the set protocol at prefix level and pointing into its sets. There is deliberately no second mutable HA `head.json`. When data and coordination containers differ, logical adjacency does not imply the same container. This uses `Sets::coordination` (`src/server/cloud_backup.rs:990-993`); manifests/object paths already derive from `set_dir`, `manifest_key`, `object_key` (`:999-1017`). Legacy `archive.json` remains the authority-off head only.

Proposed JSON shape (field values below are illustrative):

```json
{
  "schema": "vlite.writer-authority/1",
  "installation_id": "installation-identity",
  "scope_id": "dedicated-database-identity",
  "lineage": "0123456789abcdef",
  "writer_id": "opaque-process-incarnation",
  "epoch": 7,
  "revision": "unique-operation-id",
  "phase": "recovering",
  "base": {
    "epoch": 6,
    "set_id": "previous-baseline-id",
    "manifest_sha256": "<64 lowercase hex>",
    "index_sha256": "<64 lowercase hex>",
    "sequence": 123,
    "chain_sha256": "<64 lowercase hex>"
  },
  "head": null,
  "claimed_unix_ms": 1795032000000
}
```

`phase` is `recovering | serving`. In `serving`, `head` has the same reference shape as `base`, with `epoch == authority.epoch`; `base` is the frozen recovery anchor from the previous generation, or null for a first initialization. A baseline-only head may have null `index_sha256` and its database hash as `chain_sha256`. Store no ETag inside the object: retain the exact GET/PUT ETag out of band. Validate schema, bounds, identifiers, canonical hashes, positive `u64` epoch, checked increment and reference/lineage consistency; deny unknown fields. Timestamps are diagnostic, never leases or ownership evidence. `revision` disambiguates retries/unknown outcomes, not authority generation.

`writer_id` must be a fresh, random **process-incarnation** ID; a reusable ACA app/revision name is only an optional log label. Never let two processes adopt the same ID from a copied config. Reuse the existing randomness mechanism (`random_hex`, `src/server/cloud_backup.rs:325-330`) or its existing `getrandom` dependency; no new UUID crate. A restart is a new claim, not automatic reuse.

### Creation, claim and transfer

Add `AuthorityStore::create`, `claim`, `verify_writer`, `advance_head` in
`src/server/authority.rs`. Reuse `ObjectStore::get/put`, `Blob.etag`, `Cond`
(`src/server/cloud_backup.rs:148-172`), with bounded metadata reads.

* First initialization, explicitly requested for an empty authorized scope:
  `store.put(authority_key, encoded_E1, Cond::Create)` =
  `PUT ... If-None-Match: *`. Do not automatically create a missing authority
  for an already-running HA scope: deletion must fail closed, not reset E.
* Startup claim against an operator-prepared `recovering` authority:
  read the expected epoch/revision, generate this process's writer ID, and
  `put(..., Cond::Match(&observed_etag))`. Only one process can consume that
  exact prepared revision. The prepared writer ID is null in this one
  narrowly defined pre-claim state; a claimed record always has a nonempty ID.
  Add this schema variant explicitly, rather than accepting arbitrary null IDs.
* Transfer/restart: operator command, not the serving process, freezes the
  observed head into `base`, increments E, clears `head`, prepares an unclaimed
  `recovering` record, and CASes the **same key**. Require expected E/revision,
  printed loss-report acknowledgement and infrastructure-fence confirmation.
  Conflicts require a fresh report and explicit new attempt; never increment
  in an unbounded “win election” loop.
* If the old writer advances the head before transfer, its ETag wins and the
  transfer fails; the operator re-reads/reports. If transfer wins, the old
  writer's cached ETag cannot advance the head. Concurrent claims similarly
  have one winner.

Treat `Conflict` and `Unknown` separately. After an ambiguous PUT, GET and match the complete intended record/revision and writer ID. Return success only for that exact operation, never merely “same epoch.” If a later revision is present, reconcile it without overwriting; on unavailable readback, remain not ready and acknowledge nothing. `create_verified` supplies the existing bounded exact-byte reconciliation pattern (`src/server/cloud_backup.rs:930-951`). Do not reuse `ensure_owner` as election: it only binds the installation to a prefix (`:1037-1056`).

### Storage preconditions and refusal policy

Azure already emits the exact necessary headers in `condition`
(`src/server/cloud_backup/azure.rs:567-572`); `Container::put` passes them to a
single BlockBlob PUT (`:632-636`). `classify` maps HTTP 412 and BlobAlreadyExists
to `StoreError::Conflict` (`:350-365`); `etag` preserves server strings
(`:377-384`). No SDK/crate is needed and the `ObjectStore` trait need not grow.
Extend `Sets` with bounded immutable-index read/create helpers and an authority
connector, not a pretend cross-object conditional-write API.

Azure primary endpoints support strong consistency and ETag optimistic
concurrency. Microsoft documents that a changed ETag makes `If-Match` fail
with 412: [Manage concurrency in Blob Storage](https://learn.microsoft.com/en-us/azure/storage/blobs/concurrency-manage),
fetched during this investigation. This is documentation evidence, not an
Azure execution result. Existing connector/auth selection is
`azure::connect_with` (`:97-141`).

**Local directory:** no supported local authority backend can presently be qualified from this code. Reject local-path/`file:` authority targets at config validation with `authority_backend_unsupported`; never downgrade to unconditional write, timestamp comparison, read-then-rename, process-local mutex, or test `Memory`. A local database may still use Azure authority. If a directory ObjectStore is added later, require real inter-process compare-and-replace under one persistent lock plus crash-safe atomic replacement and durability, test it separately, and refuse network shares pending storage qualification. The existing restore `Staging::create` lock is only private scratch ownership (`src/server/cloud_recovery.rs:359-379`), not this protocol.

## 3. Epoch-bound batches and the head CAS

Extend archive metadata, not the database log/on-disk format:

* Introduce explicitly versioned HA-compatible manifest/index variants.
  `Manifest` currently denies unknown fields and names hashed objects
  (`src/server/cloud_backup.rs:581-600`); `Checkpoint` and `Index` also deny
  unknown fields (`src/server/cloud_backup/archive.rs:44-71`).
  Preserve exact legacy serialization when authority is absent. Do not silently
  append optional JSON keys to authority-off manifests.
* HA baseline manifest, every checkpoint entry and each immutable Index carry
  `writer_id`, `epoch`, authority scope/lineage. Index records the prior head's
  digest. Extend `chain` (`archive.rs:141-143`) with an HA-domain-separated
  encoding covering epoch, writer ID, base sequence, end sequence and segment
  hash. Recovery bundles already bind the checkpoint chain hash through
  `check_bundle` (`:147-161`); retain that protection and policy/memory checks.
* Segment payloads remain existing `LogSegment` bytes, named by SHA-256.
  Their authenticated checkpoint envelope supplies E; do not alter VLSEG001 or
  create a second replication protocol. `Index::check` validates every entry
  against the manifest and chain (`:166-202`).

Proposed `Sets::append_checkpoint` HA branch:

1. Fetch/verify claimed authority and its ETag; require this writer ID/E,
   expected current head/sequence, and `phase == serving`.
2. Prepare segment, recovery bundle and next Index with that same E/ID.
   Upload all as immutable create-only objects; verify exact hashes and lengths.
   Existing `Sets::upload` does payload readback (`cloud_backup.rs:1121-1162`);
   preserve `authorize()` shutdown checks but do not call them the CAS fence.
3. Build the next combined authority record with the new immutable Index
   pointer and a unique revision. Advance via
   `coordination.put(authority_key, next_bytes, Cond::Match(&etag))`.
   This is the **head If-Match CAS**, not an update to legacy `archive.json`.
4. On 412, re-read. Different writer/E => enter Fenced immediately; never retry
   the prepared index under the successor's ETag. Same writer/E with a different
   head => serialize/rebase from that committed head, not overwrite it.
   On `Unknown`, accept only verified exact publication; otherwise report
   unresolved and do not advance archive progress.

The process-local head publisher should serialize head updates; authority
polls/pre-ack reads need not serialize with it. For first baseline publication,
`advance_head` additionally requires `recovering` with this claimed writer ID
and CASes to `serving`. Existing set `publish/finish` can prepare a Published
baseline, but only the combined authority CAS makes it HA-authoritative
(`cloud_backup.rs:1064-1105,1187-1221`). An orphan Published set must never be
selected by “newest set” alone in HA mode.

**Proof boundary:** an old process may still create unreferenced blobs with its
storage identity. It cannot attach them to the authoritative history after
the bump. The requirement concerns publication of referenced history, not
revocation of an otherwise valid Azure credential. A blob created before the
bump and a head CAS ordered before the bump are legitimate frozen history.

## 4. Reader rejection and epoch transitions

Add an explicit authority context to HA `open_archive`, `read_index`,
`follow_archive`, and `apply_segments`; legacy callers keep their old path
(`archive.rs:211,465,544,564`). HA readers:

1. Read current authority; obtain only its serving head and exact manifest/index
   digests. Pin the referenced set before fetching, then re-read authority to
   ensure the selected E/head is still authorized. Do not use LIST ordering as
   authority; `Sets::list` is inventory only (`cloud_backup.rs:1224-1307`).
2. Require candidate manifest/Index/checkpoint `epoch == current E` and
   matching writer/scope/lineage. Reject lower E, missing stamps or mismatched
   IDs before `ReplicaFollower::apply`. Unexpected higher E triggers a fresh
   authority read, never optimistic adoption.
3. Verify current authority again immediately before each apply batch and
   before publishing the advanced generation. If E changed during download,
   discard the candidate, fail readiness and require re-enrollment; an
   unserved scratch file may have advanced, but must never become served.
   Network reads cannot make the final remote-check/local-apply interval
   atomic; disclose this race and prevent exposing a candidate once a newer E
   is observed.
4. Extend `Follow::Applied/Current` to carry the verified E/ID/head identity
   (`archive.rs:621-628`). `reader_apply::Progress::confirm` must record the
   generation's sequence **and epoch**, not just the latest observed authority
   (`reader_apply.rs:56-68`). Commit progress and generation identity under
   the generation-swap lock; unrelated atomics can produce mixed position/E.
   `swap_reader_generation` already validates read-only/family/format/graph
   compatibility (`:121-156`). Carry the captured generation stamp through
   each request so a concurrent swap cannot relabel an older read as newer E.

**Historical recovery exception, not stale-tail acceptance:** after E increments,
the new writer must reconstruct the exact frozen `base` from prior epochs.
Use a separate recovery-only API that verifies that frozen reference and
cannot serve data or discover an old mutable head. It may read historical E
explicitly named by `base`. Then publish a fresh baseline stamped E and CAS a
serving head E. Live readers never consume old-E tail entries as if current.
Until the fresh E baseline exists, they fail readiness rather than relabel the
old generation. This conservative baseline-per-epoch design avoids a mixed-E
tail proof and costs one baseline upload/copy per manual promotion.

Reuse `Sets::pin/fetch_with/release` with capacity checks
(`cloud_backup.rs:1364-1403,1438-1450,1487-1513`).
`unpin(force)` is operator cleanup, not routine reader ownership (`:1407-1435`).
Keep old and new set pins through verification/swap; release old pins only
after the old copy is retired, and report release failures explicitly.
Do not force-remove live pins. Account for the 16-pin cap (`:1392-1396`);
whole-set pins are the delegated default (PRD `:519,522`).
`Progress::fail` and `reader_failure` already provide sticky first-failure
logging/readiness behavior (`reader_apply.rs:34-47,107-112`); additionally gate
reads, not only `/ready`.

## 5. Startup, per-batch verification and self-stop

### Startup sequence and opt-in

Propose one new `--authority-file <json>` flag, valid only with explicit
writer/reader mode. JSON names existing `target_file`, expected scope,
`poll_interval_ms`, `verify_timeout_ms`, `readiness_propagation_ms`,
`drain_timeout_ms`, and writer claim E/revision when applicable. Require
positive bounded settings, no invented qualified timing defaults.
Use the existing emulator key-file mechanism; production uses managed identity.
No authority secrets are necessary. Never put signing keys in this file.

Writer startup: validate all config/refusals before opening data; connect and verify scope/installation; consume exactly one prepared claim; recover the frozen base into private staging; verify database/bundle/memory; create new E baseline; CAS serving head; install shared runtime; start periodic verification; only then serve readiness/data. Empty first initialization must be explicit. A failed startup leaves not-ready authority state and recoverable pinned objects, never silently falls back to local stale files.

Extend `vlite-server::config/open/run/serve`
(`src/bin/vlite-server.rs:183,600,762,942`) while preserving standalone.
`run` currently binds before opening and starts trust before data
(`:776-797`); bound sockets must not be served as ready during recovery.
Refuse authority with in-memory durability, insecure-local, unqualified
encrypted archival, and gRPC until its acknowledgement path is covered.
Do not turn every existing `--serve-mode writer` into mandatory HA: writer
without authority remains the existing mode with null epoch.

Reader startup does read-only authority verification, pinned seed, two private copies and verified-generation publication; it never claims or bumps E. Coordinate reader CLI work with ITEM-019 rather than inventing a second reader-apply config. The currently unconditional trusted-reader refusal can be lifted only when this complete path, role checks and epoch reporting exist.

### Exact batch boundary and overhead

“Batch” here means **each successful mutating request/acknowledged operation**,
including atomic REST batches, imports, graph/KV/memory operations and
mutation-starting admin jobs; not the archive timer's checkpoint.
Conservatively perform one fresh, uncached authority GET per successful
mutating request immediately before releasing success, even in a durability
group. Sharing a cached periodic verification does not satisfy OD-002.
Use an initial local gate before enqueue, a gate in queued workers, and a
post-commit fresh E/ID verification before acknowledgment.

`Service::run_mutation` either delegates to group commit or `run`
(`src/server.rs:638-659`); `run` returns from a blocking operation (`:661-692`).
`group_commit::commit` releases per-job outcomes after durable-group completion
(`src/server/group_commit.rs:304-395`). Put pre-ack checks before those outcomes
are released. Add a final mutating-response barrier in `observe`
(`src/server.rs:1460-1640`) for routes not using those workers and for paused
response tasks; avoid double-checking one request with a request-scoped receipt.
Do not assume `run` is mutation-only, and inventory async/background and
streaming routes so headers are not sent before their success is fenced.
Already-started background jobs must carry/check the same authority context.
Refuse any HA operation without a proven acknowledgment boundary.

On failed pre-commit verification, return 503 `writer_authority_unavailable`
without mutation. On failed **post-commit** verification, a write may already
exist locally: return 502 `write_outcome_unknown`, not a safe-to-retry failure
or false success. Preserve validation failures. No rollback is implied.
Existing `gateway::ambiguous_write` expresses that contract
(`src/server/gateway.rs:509-515`).

Estimated cost, **not measured**: at least one remote metadata GET round trip
per successful write request, small JSON parsing, and approximately one read
transaction per request; startup/publication/periodic reads are additional.
With N independent concurrent requests, one check per request means N reads,
not one per fsync group. A final stale-receipt refresh adds reads only if needed.
Authority/head mutations additionally cost immutable index upload/readback
plus one CAS PUT per archived checkpoint. No numeric latency is asserted.

MEAS-006 without a new benchmark campaign: extend existing targeted fencing
tests to count GETs and time `verify_writer` using `Instant`, collect a bounded
in-memory latency sample, and print sample count/p50/p95/max plus end-to-end
ack latency for the same tiny fixture with authority off/on. Use the existing
loopback/Azurite fixture if available; fake-store timing measures plumbing
only. Inject higher E before final check, after commit and during a paused
response; record observation-to-gate-close and observation-to-last-success
intervals with deterministic barriers, not sleeps. Separately label the
check-to-ack gap and no-load fixture results. No ACA or real-Blob extrapolation.
The existing comparative test is prior art, not a required new campaign
(`src/server/group_commit.rs:1563-1623`).

### State machine

```text
Disabled
Starting -> Claiming/Recovering -> Serving
Serving -> AuthorityUnavailable -> Serving (only same E/ID, no uncertain commit)
Starting/Recovering/Serving/AuthorityUnavailable -> Fenced -> Draining -> Stopped
```

On verified greater E **or any different writer ID**, latch Fenced permanently:
atomically close mutation admission, suppress outstanding success replies,
set `begin_drain`, invalidate readiness, stop schedulers/publishers, and signal
the binary's shared stop channel. No auto-reclaim or rollback to Serving.
An observed regression/lost authority/corrupt authority fails closed; regression
or disappearance is terminal because current authority cannot be trusted.
Transient unavailable verification immediately blocks acks/readiness; retry only
within configured bounded shutdown policy. A post-commit uncertainty drains and
stops rather than serving an unacknowledged fork.

**Ordering:** gate closure and readiness refusal occur together at discovery,
before any wait. “Drain” means finishing/closing existing work, not accepting
new writes during readiness propagation. This is intentionally stricter than
ordinary signal drain. Let committed work complete local cleanup but never
send a success after the fenced latch; unstarted queued work is refused.
Stop serving data reads from a known-fenced writer too.

`Service::begin_drain/draining` currently only set/read a flag
(`serve_mode.rs:98-105`); `admit` ignores it. Add a distinct latched authority
state so signal drain can keep its existing behavior, while fenced admission
cannot be bypassed. `ready` must check authority before database/trust success
(`src/server.rs:2373-2395`). Retain authenticated status/health diagnostics.
`serve` must select internal authority stop as well as OS signals; today its
signal task alone calls `begin_drain` and then sends `stop_tx`
(`src/bin/vlite-server.rs:954-974`).

Drain within a configured deadline, then close listeners and call
`Service::close` (`src/server.rs:739-767`). Blocking Rust workers cannot be
cancelled by dropping a future: a hung drain requires a final process exit
after deadline, never readiness resurrection. Recommend exit **1**, already
the binary's failure convention (`src/bin/vlite-server.rs:1101-1124`), with
distinct structured events `writer_fenced`, `writer_drain_timeout`,
`writer_stopped`, local/observed E and sanitized ID/scope.
Do not log tokens, bundles, data or raw store/config bodies.
The periodic poller continues while draining until the terminal stop path;
it never renews authority or changes E.

Residual limitation: checking remote authority cannot atomically send a local
HTTP ack. A process can pause after its last valid check and resume after the
bump; the gateway may itself still hold old directory E. No finite global
zombie-ack bound follows from timer settings alone. Preserve the infrastructure
fence, measure the observation-to-stop bound, and keep GATE-005/automation
closed until the actual proof obligations are demonstrated (PRD `:257-260`).

## 6. Status, response stamps and gateway behavior

For authority-enabled writer/reader responses, stamp exactly one canonical
decimal `X-VLite-Epoch`. It is the request's verified writer/served-generation
E, **not** the latest remotely observed E. A fenced E7 process learning E8
must never stamp E8. Stamp errors as well as successes where a local claimed
or served generation exists; pre-claim startup serves no application responses.
Capture a reader's stamp with its database generation, including detached
snapshot reads (`Service::run_read_then`, `src/server.rs:706-732`).
Add the header in `observe`'s response finalization, alongside existing headers
(`src/server.rs:1629-1639`), using a request/generation receipt rather than an
unrelated global atomic. No header when authority is absent, including legacy
writer/reader and shared-pool requests.

`serve_mode::status` replaces null with local E only when authority configured.
Keep existing null and JSON shape for HA-off. HA-only status may additionally
report authority state, last verified age and sanitized last error, with
observed E separately named; keep existing archive progress actual, not inferred.
Memory-bound positions/error-message redaction stays in `archive_report`
(`serve_mode.rs:174-190`) and must not be undone by logs or new loss reports.

In `Gateway::forward`, after receiving upstream headers **before spawning
`pump` or exposing response status/body** (`gateway_mode.rs:480-495`):

* If `target.epoch` is present, require exactly one syntactically valid bounded
  `u64` header, no duplicate/list/whitespace/sign/comma variants. Read the
  effective forwarded headers so `Connection: X-VLite-Epoch` cannot make a
  checked stamp disappear on the outgoing response.
* Call `gateway::check_epoch(expected, stamped)`; it already rejects missing
  and older E and accepts newer E when the directory lags
  (`gateway.rs:550-559`). Preserve that newer-E rule, with audience/lineage
  authentication still enforced independently.
* Missing/malformed/stale => read gets 503 `backend_epoch_stale`; sent write
  gets 502 `write_outcome_unknown`, via the existing operation-specific
  `EpochRefusal::error` (`gateway.rs:529-542`). Drop upstream, no retry.
* For long requests, consult the freshest available control-state snapshot
  before forwarding the result; use at least the maximum of dispatched E
  and the current same-tenant/lineage E. Fail closed if routing changed lineage,
  disappeared or control state became unusable. This reduces an avoidable
  in-flight stale-directory window, not the fundamental check/ack gap.

Strip inbound `X-VLite-Epoch` explicitly in `request_headers`
(`gateway_mode.rs:662-685`); it currently passes arbitrary `x-vlite-*` except
the principal header. Strip only authority-owned names, not an unreviewed
blanket removal of every `x-vlite-*`. Keep the backend stamp in
`response_headers` (`:689-702`), subject to safe hop-by-hop rules.
The header is not a signature or credential: keep gateway envelopes and
backend checks (`Trust::authenticate`, `gateway_trust.rs:225-255`).
Headers cannot stop a stream whose response head was already sent; terminate
it on fencing where possible and document that limitation. Do not claim
retroactive revocation of delivered bytes.

HA-off AC-010 means no added E header, response fields, authority reads, errors,
startup requirements or archive bytes. Preserve legacy gateway `epoch: None`
behavior. Compare response bodies/status/semantic headers exactly; normalize
only already-variable request ID/date/uptime in test fixtures, not arbitrary
differences. New CLI dispatch must preserve database-name compatibility.

## 7. File/function plan and safe parallel ownership

**Contract first (one engineer, short serial step):** agree schema variants,
`AuthorityStore`, immutable `HeadRef`, per-request/generation receipts,
`verify_writer`, `advance_head`, latched stop notification and error mapping.
No runtime contributor should independently invent another authority schema.

Then three disjoint write scopes:

| Owner | Files exclusively owned | Concrete changes |
|---|---|---|
| A: storage/protocol | new `src/server/authority.rs`; `src/server/cloud_backup.rs`; `src/server/cloud_backup/archive.rs`; `src/server/cloud_backup/azure.rs`; `src/server/backup_policy.rs`; `src/server/cloud_recovery.rs` | Claim/verify/CAS types; immutable HA indices; HA manifest/chain variants; checkpoint and baseline head CAS; current-E read enforcement; frozen-base recovery; actual progress only after confirmation; backend refusal and storage deadlines. Unit tests inline. |
| B: backend/lifecycle | `src/server.rs`; `src/server/serve_mode.rs`; `src/server/reader_apply.rs`; `src/server/group_commit.rs`; `src/bin/vlite-server.rs`; `src/server/gateway_trust.rs`; new `tests/server_authority.rs` | Module/export wiring; config/startup claims; per-request/worker/group ack gates; epoch/generation receipts; read/ready admission; poll/self-stop; status/header; CLI operator preparation; trusted-reader and publisher integration. |
| C: edge/contracts | `src/server/gateway.rs`; `src/server/gateway_mode.rs`; `tests/gateway_azurite.rs`; new `tests/authority_azurite.rs`; `docs/API.md`; `docs/SECURITY.md`; `src/openapi.json`; `site/api.html`; affected approved status snapshots | Strict stamp parsing; pre-stream check; preserved ambiguity; header filtering; fake-backend and Azurite integration tests; public contract/docs updates. |

B may add exported wrappers for the binary because `pub(super)` authority types
are inaccessible across the library/binary crate boundary; A owns implementation.
A and B coordinate backup-runtime attachment through the agreed contract, not
concurrent edits to the same file. Integration follows A's protocol then B/C
end-to-end tests. With two engineers, combine B/C sequentially; do not split
`server.rs` ownership across workers. Never touch `deploy/`.

**Resolve trusted writer/publisher mismatch explicitly:** retain the existing
`--gateway-trust-file` prohibition on `VLITE_ADMIN_DIR` for normal backends.
For authority-enabled writers, add a private `runtime_dir` and immutable archive
policy reference to `authority-file`, attach the existing backup runtime through
a dedicated initializer, and supply its capture metadata without creating a
local credential/session authority. Reuse `Runtime::open_with`,
`archive_once`, `publish` and `authorized`
(`backup_policy.rs:839,1364,1605,1035`) and add the E guard to their handles.
Refuse startup until that attachment is available; do not merely bypass the
current refusal. `authorized` today fences only local shutdown state.
This is necessary integration work, not evidence that such a path exists now.
Installation identity must persist in authority/configured runtime consistently
with the prefix owner; do not rekey a new installation on every restart.

Operator CLI proposal: `vlite-server authority init|prepare-transfer|status`.
Initialization/transfer require target/scope, expected E/revision, printed
loss-report acknowledgement and a fence-confirmation artifact. The artifact
must record independently verified old-app replica-zero/fence evidence;
a boolean `--yes` is not proof of infrastructure fencing. Reuse existing Azure
storage/auth and operator tooling, no cloud control-plane service or automatic
promotion. If infrastructure state cannot be verified with permitted tools,
the command must refuse preparation; status/reporting remains available.
This is ITEM-025 scope and must not be represented as satisfied by ITEM-023/024.

Documentation must update the explicit “no epoch fencing” and stored-but-unchecked
statements (`docs/API.md:944-948,1223-1229`), readiness/drain behavior
(`:1118-1123`), and the OpenAPI status description (`src/openapi.json:94`).
Add a real serve-status API explanation: the inspected `docs/API.md` currently
only references that endpoint in its lineage note (`:1221-1222`).
Keep admin authorization and memory redaction. Security docs must retain
least privilege, no-secret logging, rekey-only public restore and hashed-not-signed
archive limitations (`docs/SECURITY.md:303-379`). A process with both container
write rights can forge a self-consistent history; epochs do not fix that threat.

## 8. Proposed tests (not executed)

Use the existing deterministic `Memory::fault/hook` fixture
(`src/server/cloud_backup.rs:1969-1982`) for races and Unknown outcomes.
Use existing Azurite integration fixtures as an optional second tier
(`Cargo.toml:131-143`); real Azure remains a separately authorized tier.

| Proposed test name | One-line assertion |
|---|---|
| `authority_create_once` | First create is E1; a second create cannot replace identity/E/head. |
| `authority_claim_consumes_prepared_revision` | Only the intended prepared E/revision can acquire a fresh process writer ID. |
| `authority_double_claim_has_one_winner` | Barrier-synchronized startup claims yield one serving claimant and one refusal. |
| `authority_copied_config_is_not_same_writer` | Two copies generate different incarnation IDs and cannot share ownership. |
| `authority_epoch_checked_increment` | Overflow, zero, regression, malformed scope/hash/schema fail closed. |
| `authority_unknown_claim_readback` | Exact committed revision reconciles; different/missing/unreadable content never becomes success. |
| `authority_missing_does_not_reinitialize` | Deleting authority blocks a configured writer instead of resetting E. |
| `head_epoch_bump_invalidates_old_etag` | Pause old writer after verify; transfer same key; old head PUT gets Conflict and no referenced history changes. |
| `head_cannot_escape_through_new_baseline` | An old writer's new Published set/index remains undiscoverable through authority. |
| `head_unknown_commit_requires_exact_reference` | Maybe-applied head is progress only after exact readback; conflicting publication is not counted. |
| `reader_rejects_stale_epoch_before_apply` | E6 candidate under E7 authority does not invoke follower apply or generation swap. |
| `reader_epoch_moves_during_download` | Higher E between fetch and apply/swap invalidates the candidate and readiness. |
| `reader_frozen_base_is_recovery_only` | E7 writer may reconstruct explicitly frozen E6 base; live E7 follower cannot use E6 tail. |
| `reader_stamp_tracks_generation` | Read captured before swap keeps its original E/sequence, never the replacement's stamp. |
| `reader_pins_survive_candidate_failure` | Failed seed/swap does not release the active set pin; failed cleanup is surfaced. |
| `writer_checks_each_mutating_ack` | Vectors, batches, graph, KV, memory, imports and supported admin-job acks each perform fresh E/ID verification. |
| `writer_group_checks_each_success_reply` | Coalesced fsync does not bypass per-request verification; original validation failures stay intact. |
| `writer_fenced_after_local_commit_is_ambiguous` | E bump before ack suppresses success and reports unknown outcome, without promising rollback. |
| `writer_precommit_unavailable_has_no_effect` | Authority outage before execution refuses new work and leaves the database unchanged. |
| `writer_fenced_latch_stops_queue_and_readiness` | Discovery immediately closes admission/readiness and suppresses queued success through drain. |
| `writer_fenced_exits_nonzero` | Internal fence stops listeners and exits 1 with structured reason, even without an OS signal. |
| `writer_drain_deadline_is_terminal` | A deliberately blocked worker cannot keep readiness or resurrect serving after the deadline. |
| `writer_periodic_check_fences_idle_process` | No traffic is needed to discover loss and self-stop. |
| `epoch_header_enabled_only_with_authority` | Writer/reader success/error has one local generation stamp; authority-off has none. |
| `gateway_checks_epoch_before_stream` | Missing/duplicate/malformed/older stamp sends no upstream success bytes; newer stamp follows existing acceptance policy. |
| `gateway_epoch_refusal_never_retries_write` | Sent write maps to unknown outcome and backend call count stays one. |
| `gateway_strips_forged_epoch_request` | Caller epoch cannot influence backend authority or gateway response verification. |
| `gateway_connection_nominated_epoch_refused` | A hop-by-hop nominated stamp cannot bypass the required outgoing epoch contract. |
| `ha_off_wire_and_archive_unchanged` | Legacy responses, null status E and manifest/index bytes match baseline except known nondeterministic values. |
| `authority_local_backend_refused` | Local-path authority config fails before touching a database or creating files. |
| `transfer_requires_fence_and_loss_ack` | Missing/stale evidence or head CAS conflict prevents E bump and requires fresh loss acknowledgement. |

Coverage maps to FM-001/002 (claim/zombie), FM-003/005 (outage/Unknown),
FM-006 (reader refusal), AC-007 (publication/ack fence), AC-009 (actual progress)
and AC-010 (off-path compatibility). Tests must include deterministic pause
points after remote check and before response send, not only a sequential
“bump then write” scenario.

## 9. Risks, recommended defaults and unresolved evidence

| Risk/question | Recommended default allowing implementation to proceed |
|---|---|
| Two separate mutable objects cannot atomically fence | Combined authority/head CAS above; immutable per-batch indices. Do not ship separate authority-GET/head-PUT as proven fencing. |
| Check-to-ack pause admits a residual window | Manual infrastructure fence mandatory; no zero-window or finite pause-independent ack guarantee; automation stays disabled. |
| How often to poll / how long to drain? | Required positive configuration, unmeasured; pick test fixture values explicitly, never label them qualified operational defaults. |
| Blob timeout is currently large | Existing `client` uses 120s request/10s connect and GET retries (`azure.rs:144-152,401-412`); add a dedicated bounded authority-client option and stop-aware retry budget without changing legacy backup timeouts. Async timeout alone cannot cancel a blocking call. |
| FM-003 says writer keeps serving during archive outage | Preserve that for payload-only archival failures with healthy authority. Authority-read failure blocks acks; if the same outage removes authority too, availability is deliberately lower under OD-002. Document this distinction. |
| Reader recovery needs historical E | Frozen-base recovery-only API plus a fresh baseline at every epoch; never exempt all old tail entries from live checks. |
| Mixed metadata versions / accidental HA disable | Explicit HA schema variants; HA metadata cannot be silently opened as legacy. No in-place downgrade of an established authority scope. |
| Retention might delete current/base objects | Hold operation pins before reference publication/transfer, reconcile interrupted operations, retain base pins until safe new baseline. Surface the existing pin-cap limit; do not implement expiring pins casually. |
| Current cloud restore rekeys lineage | Do not call public `cloud_restore` a promotion primitive. `restore_pinned` restores/rekeys admin state (`cloud_recovery.rs:628-649` and `docs/SECURITY.md:339-355`); add the protected frozen-base seed path required above, with preserved lineage and separate authorization. |
| Trusted backend lacks archive admin runtime | Dedicated private publisher initializer, no local auth fallback; startup refusal until wired. This is a material integration dependency, not a one-line flag relaxation. |
| Reader trusted authentication | Only lift CLI refusal after read-only envelope checks and exact generation E/lineage mapping are tested; shared-pool stays legacy. |
| Geo/immutability/proxy semantics | ZRS primary endpoint only for authority; no geo failover, replication of mutable authority or WORM lock on it. Do not infer cloud behavior from Azurite. |
| Storage writer is not Byzantine-fenced | Least privilege and private backends remain mandatory; E protects protocol participants, not an attacker controlling both data and authority writes. |
| Infrastructure-fence evidence cannot be verified | Refuse transfer, leave E unchanged; reporting/design may proceed, but do not claim ITEM-025 or GATE-005 passed. |
| Multi-engineer scope is larger than ITEM-023/024 alone | Deliver protocol + backend gates first, then reader/gateway + manual-tool integration; no HA completion claim for a library-only slice. |

## 10. Checks actually executed and validation boundary

**Executed:** read-only repository inspection using `view`, `rg`, `glob`;
`git rev-parse HEAD`; initial `git status --short` (clean); tracked-path inventory;
Microsoft Learn fetch cited above. Initial requested `src/cloud_backup.rs` and
`src/archive.rs` probes found those paths absent; inspection used the actual
`src/server/cloud_backup.rs` and `src/server/cloud_backup/archive.rs`.
Final verification confirmed the only untracked file is this report and there
are no tracked-file diffs. Strict UTF-8 decoding succeeded; the report is
nonempty, below 1 MiB and below approximately 600 lines.

**Static inspection only:** every implementation finding, protocol proof sketch,
call-site inventory and file/function plan above. **Fixture-only checks: none
executed.** The test table is proposed coverage, not observed test success.

**Not run:** Cargo build/test, Azurite integration, real-Azure conditional-write
drills, process-pause/failover tests, MEAS-006 latency/zombie measurements,
ACA secret delivery or deployment qualification. The brief explicitly requires
no execution validation beyond report-only git status; implementing or validating
the proposed fencing is outside this report-only assignment. No required check
is represented as passed by report delivery, and no numerical bound is measured.
GATE-004/005/007 remain unproven by this investigation.
