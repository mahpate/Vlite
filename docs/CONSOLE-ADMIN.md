# Console administration API

Reference for the server administration routes behind the dashboard. For
sign-in and roles, see [dashboard sign-in and accounts](SECURITY.md#dashboard-sign-in-and-accounts).
For TLS, tenants, credential limits and tracing, see [SECURITY.md](SECURITY.md).

Every route below needs authentication; only the static console files are
public. The admin token (`VLITE_AUTH_TOKEN`, `VLITE_AUTH_TOKEN_FILE` or
`--token-file`) is the super admin credential for automation and recovery.
The server never returns bootstrap secrets. Creating or rotating a scoped
credential returns its secret once, so never log that response.

## Configuration and roles

- `VLITE_ADMIN_DIR` enables dashboard accounts, tenants, backups, restore,
  webhooks and disk ANN. Use a private, writable, persistent directory that
  untrusted local users cannot change. Snapshots in it contain all database data.
- `VLITE_READ_ONLY_TOKEN` or `VLITE_READ_ONLY_TOKEN_FILE` adds a read-only
  credential. Set one source only; an empty file disables it. It needs an
  admin token.
- `--insecure-local` is loopback-only. Every caller is admin and there is no
  sign-in.
- `VLITE_TRUSTED_PROXY=1` is for when a TLS-terminating or local reverse proxy
  fronts the API. Password sign-in otherwise needs HTTPS or a loopback-only
  listener. Docker Compose sets it.
- `VLITE_SESSION_IDLE_MINUTES`, `VLITE_SESSION_MAX_HOURS`, `VLITE_ADMIN_USERNAME`
  and `VLITE_ADMIN_PASSWORD_FILE`: see [sign-in settings](SECURITY.md#dashboard-sign-in-and-accounts).

The server, not the console, enforces access:

- Read-only callers can use GET and HEAD outside admin routes, plus searches
  that cannot build an index: exact, `approximate_ready`, `auto` and hybrid.
  Lazy `approximate` can build a graph, so it returns 403.
- Scoped credentials also restrict collections and operations. Lazy search
  needs both `search` and `search_build`.
- `/v1/admin/` routes need a super admin, except replication export, which
  accepts database-wide read access.
- Failed authentication returns 401. Requests admitted before a token rotation
  or revocation may finish.

`GET /v1/capabilities` returns `version`, `api_version`, `format_version`,
`role`, `features`, `routes` and `identity`. Check `api_version` and
`features`, not the package version.

- `role` is `admin`, `read_only` or `scoped`. Dashboard accounts report
  `admin` for super admins and `scoped` otherwise. Scoped responses add the
  effective collections, operations and expiry.
- `features` booleans reflect configuration; see
  [CAPABILITIES.md](CAPABILITIES.md). With `filtered_ann`, approximate modes
  accept `filter`; without it, send filters only in exact mode.
- `routes` lists every supported route template.
- `identity` is for display only. `kind` is `open`, `super_admin`,
  `read_only`, `scoped` or `user`; `label` is a plain summary such as
  `API key "ci" (<id>) · tenant "Acme"`. Scoped identities add the credential
  and tenant IDs and names; user identities add the username, account role
  and tenant. Neither includes secrets. Labels contain operator-chosen names,
  so render them as text.

## Dashboard administration walkthrough

Super admins work in **Administration**. Its **Users & access** tab holds:

- **Users:** create, update or delete super admin, editor and viewer accounts.
  Editors and viewers see one tenant's collections and graphs, or all of them
  (viewers read graphs only); **Graphs** and **Graph access** narrow graph use.
  Temporary passwords must be changed at next sign-in.
- **Tenants:** list tenants with their state, collection and vector counts, and
  create one with an optional vector limit. **Show usage and API keys** lists
  its collections, usage and key counts. **Save name and limit** renames it or
  changes the limit; a lower limit keeps existing vectors but refuses new ones.
  **Suspend** blocks the tenant's API keys over REST and gRPC, refuses new keys,
  blocks its users' sign-in and ends their sessions; data stays. **Resume**
  restores sign-in and unexpired, unrevoked keys. **Delete** needs the tenant's
  collections and graphs dropped first; it blocks bound keys and removes the
  tenant's user accounts.
- **API keys:** create, list, rotate with a grace period and revoke keys scoped
  to collections and operations; read the persistent audit. Optional fields
  bind a tenant ID and set requests per second (`1..1000000`), an aggregate and
  a per-collection vector limit. Blank means no limit; zero means no growth.
  The dashboard caps quotas at `9007199254740991`. A tenant's first key needs
  `*` with the `collections` operation to create collections. New secrets show
  once.

**Administration** and **Observability** also hold:

- **Health and tokens:** refresh health, see token sources, reload token files,
  revoke the read-only token and read the recent request audit. Change token
  files on the host first.
- **Monitoring:** request rates, errors, latency, a per-route table and the
  live log tail.
- **Backup and restore:** list and create snapshots, verify or drill a backup,
  stage a restored copy and select it for the next restart. Read the scheduled
  backup policy before editing it; runtime changes do not rewrite environment
  settings. No action replaces the live file.
- **Automation:** read or update the automatic index maintenance policy, or run
  one tick. Changes last until restart. Per-collection index work stays in
  **Collections → Index**.
- **Webhooks and PITR:** see
  [webhooks, disk ANN, PITR and encryption status](#webhooks-disk-ann-pitr-and-encryption-status).
- **TLS certificates:** after you replace certificate, key or client CA files on
  the host, validate and reload every file-backed TLS identity. All candidates
  validate before any swap; bad files keep the current identities. Only new
  handshakes see the change.
- **Replication export:** download a bounded log segment after a commit
  sequence. The budget defaults to 1 MiB (`1..16777216` bytes), and success
  advances the input to `last_sequence`. Read-only keys and scoped keys with
  database-wide `read` on `*` can use it. Segments contain database data;
  applying them needs an external replica workflow. Format v1–v3 files return
  409 `requires_format_v4`. A cursor past the end of the log returns 409
  `cursor_ahead`; start again from the replica's last applied sequence or 0.

Mutations ask for confirmation, run one at a time and never retry. Signing out
clears admin results. Startup settings, secret and TLS files, and format
upgrades stay host tasks.

## ANN maintenance observations and retry backoff

`GET /v1/index/hnsw/maintenance` (or its collection route) reports whether the
active graph is ready and whether pressure recommends a rebuild. The graph can
stay ready while a replacement builds. The optional `rebuild.phase` is
`building`, `cancelling`, `ready_to_publish` (finished, waiting for a poll) or
`discard_pending` (terminal, waiting for a poll); Rust exposes it as
`HnswRebuildProgress.phase`. Observing never publishes. Writes never rebuild
synchronously, and ready-only search never falls back to exact search.

Automatic server maintenance can retry rebuilds that end in catch-up overflow
or supersession. Backoff is off by default and never affects explicit
`maintain_hnsw` calls. Enable it with `PUT /v1/admin/maintenance/auto`:

```json
{"retry_backoff_initial_ms":250,"retry_backoff_max_ms":4000}
```

The delay doubles with each repeated failure for a collection, up to the
maximum. Set both values to zero to disable, or use
`1 <= initial <= max <= 3600000` milliseconds. Tick responses mark a deferred
retry as `retry_backoff` with the attempt count and remaining delay.

## Opt-in durable group commit

`vlite-server <path> --group-commit` coalesces concurrent puts, deletes, atomic
batches and metadata patches into one shared datafile sync. This covers REST,
collection routes and gRPC Put, Delete, WriteBatch and UpdateMetadata. It matters
most with `--durability full`; the default
[periodic durability](REFERENCE.md#periodic-lazy-durability) syncs every 60 s.
Set limits after the flag:

```text
--group-commit --group-commit-requests 16 --group-commit-bytes 67108864 --group-commit-delay-ms 0
```

| Setting | Default | Range |
|---|---|---|
| Outstanding grouped writes | 16 | 1–16 |
| Retained-input byte budget | 64 MiB | 1024–268435456 bytes |
| Collection window | 0 ms | 0–100 ms |

- The byte budget conservatively charges retained vectors, payloads and
  metadata; it is not a process memory limit. HTTP body limits and the 16
  request and worker limits still apply.
- A full queue or budget, or one request larger than the budget, returns 503
  `busy` at once.
- Zero delay drains queued work without waiting. A positive delay can improve
  coalescing at low arrival rates but adds latency; on the tested Windows host,
  a 2 ms wait gave roughly 16 ms sequential-write latency.
- Requests run FIFO under one exclusive owner lock. Scope, quotas, conditional
  revisions and batch validation run per request against earlier accepted
  writes. A rejected request changes nothing and does not affect its neighbors.
- Each request keeps its own frame and sequence. A group is **not** an
  all-or-nothing transaction. Readers never see a partial group, and no success
  returns before the shared sync succeeds.
- Imports, collection and admin operations, and maintenance keep their own
  durability paths and can run between groups.
- If the shared sync fails, every pending success becomes 503
  `commit_indeterminate`; validation failures keep their status. Append failures
  also poison the owner. Nothing retries automatically. An unacknowledged group
  can recover wholly or partly after a crash, so never infer absence from an
  error or disconnect.
- Graceful close stops admission and drains the coordinator before closing the
  owner. Abrupt termination does not drain.

In-process servers opt in with
`Service::with_group_commit(GroupCommitConfig::default())` before cloning or
publishing the service. Its tests cover crash, short-write and failed-sync
recovery, not power loss.

Metrics use the `group_commit_` prefix (`vlite_group_commit_` in Prometheus).
`group_commit_enabled` always reports the policy; the rest appear only when
enabled, and collection-scoped callers never see them.

- `group_commit_groups_total` and `group_commit_requests_total` count attempted
  groups and requests, not successful syncs.
- Gauges: `admitted_requests`, `admitted_bytes`, `queued_requests`,
  `peak_requests`, `peak_bytes`.
- Counters: `rejected_requests_total`, `rejected_bytes_total`,
  `rejected_closed_total`, `queue_wait_count_total`,
  `queue_wait_nanoseconds_total`, `sync_count_total`, `sync_requests_total`,
  `sync_failures_total`, `sync_nanoseconds_total`, `disconnected_replies_total`.
- Maxima: `queue_wait_max_nanoseconds`, `sync_max_nanoseconds`.

Queue wait includes the window, lock contention and earlier work in the group;
sync time starts after the last operation. These are process-local
diagnostics, not an accounting ledger.

Reproduce the local-disk comparison with:

```text
cargo test --release --features server --lib comparative_durable_write_benchmark -- --ignored --nocapture
```

It checks reopening all 512 acknowledged records, with 16-dimensional vectors
and 128-byte payloads, at concurrency 1 and 16. Two paired runs on a shared
Windows host measured zero-delay grouping at 12571–15617 writes/s versus
1926–2910 writes/s with independent syncs at concurrency 16 (5.4–6.5x); grouped
p99 was 1.52–1.65 ms versus 5.93–16.43 ms. This is a small, noisy in-process
workload, not a production SLO. Sequential zero-delay throughput was
6–8% lower; an explicit 2 ms window was much slower sequentially because of
host timer granularity. Tune against real arrivals and storage.

### Exact-search read snapshots

Simple exact searches (no reranking or indexed filter) scan a bounded snapshot
outside the owner lock, so writes can commit during the scan. When snapshot
limits are exhausted, the search runs under the lock instead of failing. If
snapshot copying hurts latency, start with `--lock-held-reads` (embedded:
`Service::with_snapshot_reads(false)`); results are the same. Metrics:
`snapshot_reads_enabled`, `read_snapshot_active_captures` and
`read_snapshot_retained_logical_bytes` (logical bytes per capture, not resident
memory).

## Token lifecycle and audit (admin only)

| Route | Behavior |
|---|---|
| `GET /v1/admin/tokens` | Returns `{generation,admin_file_configured,read_only_configured,read_only_file_configured}`. |
| `POST /v1/admin/tokens/reload` with `{}` | Reads only configured files, validates both credentials and swaps them atomically. Invalid files change nothing. An empty read-only file revokes that role; identical admin and read-only tokens are rejected. |
| `POST /v1/admin/tokens/revoke-read-only` with `{}` | Disables the read-only token in memory and increments `generation`. Empty its file to keep it revoked after reload or restart. |
| `GET /v1/admin/audit` | Returns `{items:[...],capacity:256}`. |

Environment credentials are read at startup; restart to rotate them. No route
creates arbitrary tokens or accepts file paths.

Audit items contain only
`{request_id,method,route,status,role,duration_ns,timestamp_unix_ms}`. `route`
is the matched template or `"unmatched"`;
`role` is null when authentication failed. Queries, IDs, vectors, metadata,
payloads, headers and bodies are never recorded. The ring is process-local,
resets at restart and is not a tamper-evident compliance log.

## HNSW configuration, jobs, and maintenance

`config` is `{m,ef_construction,seed}`, default `16,100,42`. `m` is 2–64 and
`ef_construction` is `m`–4096. `seed` accepts a JSON u64 or a decimal string;
admin responses use a string. `POST /v1/search` and `POST /v1/index/hnsw`
accept `config`, so ready-only queries can select a non-default graph.

`limits` is `{max_records,max_snapshot_vector_bytes}`, default
`100000,134217728`, with ranges 1–1000000 and 1–268435456. They bound the
snapshot, **not** process memory, duration or graph allocations. ANN metrics
are `"squared_l2"` and `"cosine"`.

| Route | Behavior |
|---|---|
| `POST /v1/index/hnsw/rebuild` with `{config?,metric?,limits?}` | Starts one bounded job and returns 202 `{"outcome":"running"}`, or 409 if a job is pending. |
| `GET /v1/index/hnsw/rebuild` | Observes without polling; returns `{policy,status}` like the maintenance GET. |
| `POST /v1/index/hnsw/rebuild/poll` with `{}` | Reaps or publishes a finished graph without blocking. `outcome` is `idle`, `running`, `cancelling`, `published`, `cancelled`, `superseded` or `catch_up_overflow`. A terminal result is consumed. |
| `POST /v1/index/hnsw/rebuild/cancel` with `{}` | Returns `{cancelled:boolean}`. Cancellation is cooperative and keeps the active graph; poll afterward. |
| `GET /v1/index/hnsw/maintenance` | Returns the policy and status shown below. Never publishes. |
| `PUT /v1/index/hnsw/maintenance` | Validates and replaces the process-local policy. Omitted fields use defaults. High-water bounds are 1–256 IDs and 1–8388608 vector bytes. Resets at restart. |
| `POST /v1/index/hnsw/maintenance/tick` with `{}` | Polls or publishes a matching job, then starts a replacement if needed. Returns `{outcome,started,status}`; a mismatched pending job returns 409. |

```json
{"policy":{"config":{"m":16,"ef_construction":100,"seed":"42"},"metric":"squared_l2","limits":{"max_records":100000,"max_snapshot_vector_bytes":134217728},"rebuild_at_buffered_ids":128,"rebuild_at_buffer_vector_bytes":4194304},"status":{"ready":false,"pressure":"unavailable","active_buffer":null,"rebuild":null}}
```

- `pressure` is `healthy`, `rebuild_recommended`, `capacity_exhausted`,
  `unavailable` or `catch_up_overflow`.
- `active_buffer` is null or
  `{buffered_ids,buffer_vector_bytes,remaining_ids,remaining_vector_bytes}`.
- `rebuild` is null or
  `{config,metric,snapshot_records,processed_records,worker_finished,terminal_reason,catchup_buffer}`.
  Progress counts records; it is not an ETA.

Readiness applies to the maintenance policy's config and metric. There is no
hidden timer or retry. The synchronous index POST still returns 204.

## Safe backup and restart-selected restore (admin only)

| Route | Behavior |
|---|---|
| `GET /v1/admin/backups` | Returns `{enabled,items:[{name,kind,bytes}],restart_required:true}`, where `kind` is `backup` or `restore`. Lists at most 1000 generated files. |
| `POST /v1/admin/backups` with `{}` | Snapshots while the owner lock excludes writers, then syncs, validates and publishes a new `backup-*.vlite` without overwrite. Returns 201 `{name,inspection,restart_required:true}`. Copying delays other database work, so schedule large copies. |
| `POST /v1/admin/restores` with `{"backup":"backup-....vlite"}` | Validates the backup and creates a new `restore-*.vlite`. Returns 201 with the same schema. Optional `sequence` or `timestamp_ms` selects a PITR target. |
| `POST /v1/admin/restores/select` with `{"name":"restore-....vlite"}` | Revalidates the copy and records it in the private `next-startup` marker. Returns `{name,inspection,restart_required:true}`. |

`inspection` is
`{format_version,dimensions,live_records,file_bytes,valid_bytes,incomplete_tail_bytes}`.
Without `VLITE_ADMIN_DIR`, copy and select return 409. Paths, traversal,
symlinks and overwrites are rejected, and invalid backups never replace the
active database. Do not change snapshots by hand while the server runs.

**Selection does not switch the live database.** At the next startup with the
same `VLITE_ADMIN_DIR`, the server validates and opens the selected copy instead
of its positional path. A `--create-if-missing` dimension must still match. A
bad selection fails startup; it never falls back. The original file stays
untouched. The marker persists: remove `next-startup` offline to return to the
positional database, or select another copy. Do not move or delete a selected
copy. There is no upload, download, delete or in-place recovery route.

Unix syncs the directory after copies and selection; Windows keeps the
engine's directory-entry power-loss limitation. An I/O error after publication
can leave the output present, so inspect it before retrying.

## Webhooks, disk ANN, PITR and encryption status

- **Webhooks** need a super admin and `VLITE_ADMIN_DIR`. List subscriptions
  with delivery attempts, last success and last error; view or delete one by
  its 24-hex ID. Create takes an HTTP(S) URL without credentials or fragment,
  optional collections (blank means all), event names and a 16–256-byte
  signing secret. Save the secret first: the console clears it, hides it from
  the request inspector and can never show it again. The console never sends
  your token to receivers. See [WEBHOOKS.md](WEBHOOKS.md) for signatures,
  at-least-once delivery and retries.
- **Disk ANN** (Advanced retrieval) builds a sidecar for the selected
  collection. Build needs index scope and confirmation, uses the playground
  metric and HNSW settings, and shows the sidecar's lineage and sequence.
  **Probe** needs search scope and runs a one-neighbor query. There is no
  status route. A missing or stale sidecar returns an error and never rebuilds
  silently. Sidecars need `VLITE_ADMIN_DIR` and are unsupported for encrypted
  databases.
- **PITR** restores a generated backup to a commit sequence or Unix-millisecond
  timestamp as a new copy. It never replaces the live database or selects the
  copy for startup. A sequence past the backup's log returns 422
  `invalid_restore_target`; a timestamp on a file without commit timestamps
  returns 409 `timestamps_unavailable`, so restore by sequence. The dashboard
  accepts integers up to 9,007,199,254,740,991; use an API client for larger
  u64 values.
- **Encryption badge:** Overview shows `is_encrypted` and the key ID, never key
  material. If the caller cannot read database details (including scoped
  keys), the badge says unavailable. Keys are managed on the server; see
  [SECURITY.md](SECURITY.md).
