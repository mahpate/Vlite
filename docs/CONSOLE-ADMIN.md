# Console administration API

Native HTTPS, gRPC mTLS/certificate reload, tenant isolation, persisted credential
rate/vector limits and optional OTLP tracing are documented in [SECURITY.md](SECURITY.md).

The bundled console talks to this same-origin experimental REST API. All routes
below require bearer authentication (except static console files). Existing
`VLITE_AUTH_TOKEN` / `VLITE_AUTH_TOKEN_FILE` and `--token-file` remain admin
credentials. Existing bootstrap secrets are never returned. Scoped credential
creation and rotation return a new secret exactly once; never log that response.

## Configuration and roles

- `VLITE_READ_ONLY_TOKEN` or `VLITE_READ_ONLY_TOKEN_FILE` optionally adds a distinct
  read-only credential. Exactly one source is allowed. An empty read-only file
  disables that credential. Read-only credentials require admin authentication.
- `VLITE_ADMIN_DIR` enables backup/restore administration. Use a private,
  operator-controlled, writable persistent directory. Snapshots contain all
  database data. Untrusted local users must not be able to change this directory.
- `--insecure-local` remains loopback-only; all callers have admin access in that
  explicit development mode.

Read-only credentials permit authenticated GET/HEAD outside admin routes and
non-building searches, including exact, approximate_ready, auto and hybrid
where supported. Lazy approximate can build a graph and is therefore forbidden
with HTTP 403. Scoped credentials additionally restrict collections and
operations; lazy search requires both search and search_build. All `/v1/admin/`
routes require the bootstrap admin credential. These rules are enforced by the server,
not by console controls. Authentication failure returns 401. Existing requests
already admitted before token rotation/revocation may complete.

`GET /v1/capabilities`:

```json
{"version":"0.1.2","api_version":1,"role":"admin","features":{"read_only":true,"tokens_reload":true,"audit":true,"background_rebuild":true,"maintenance":true,"backup_restore":true,"conditional_writes":true,"filtered_ann":true},"routes":["/v1/capabilities"]}
```

The example above shows legacy flags only. Current responses additionally report
`format_version` and the advanced capability flags described in
[CAPABILITIES.md](CAPABILITIES.md). `role` is `"admin"`, `"read_only"` or
`"scoped"`; scoped responses include effective collections, operations and expiry.
`routes` includes the complete supported
data/admin route templates. Feature booleans reflect configuration where relevant.
Clients should check API version and features rather than infer support from the
package version. Existing data endpoints are unchanged, except search and index
build now additionally accept optional `config`. `filtered_ann: true` means
`filter` is accepted in `approximate` and `approximate_ready` search (filter-aware
traversal, up to k eligible hits, no exact fallback); without it, send nonempty
filters only in exact mode. Collections and hybrid/import workflows require
explicit v4 creation or an offline copy-out upgrade; enabling the console does
not migrate any file.

## Token lifecycle and audit (admin only)

- `GET /v1/admin/tokens` returns
  `{generation,admin_file_configured,read_only_configured,read_only_file_configured}`.
- `POST /v1/admin/tokens/reload` with `{}` reads **only configured files**, validates
  both candidate credentials, and atomically swaps them. It returns the same
  status object. Invalid/unavailable files leave both active credentials unchanged.
  Admin files must contain a valid token; an empty read-only file revokes that
  role. Identical admin/read-only tokens are rejected.
- `POST /v1/admin/tokens/revoke-read-only` with `{}` immediately disables the
  in-memory read-only token and increments generation. To persist revocation
  across reload/restart, empty its configured file (or remove its environment
  credential offline). A later explicit reload can re-enable a nonempty file.
- Environment credentials are startup-only; restart to rotate the admin
  environment credential. There is no HTTP arbitrary-token creation or file-path
  input. Replace configured token files safely before reload; keep credentials
  outside source control.
- `GET /v1/admin/audit` returns `{items:[...],capacity:256}`. Items contain only
  `{request_id,method,route,status,role,duration_ns,timestamp_unix_ms}`. Routes are
  matched templates (for example `/v1/vectors/{id}`), or `"unmatched"`. No URL
  query, actual record IDs, vectors, metadata, payloads, authentication headers,
  or request bodies are recorded. Role is null when authentication failed.
  The bounded process-local ring resets at restart and is not a durable,
  tamper-evident compliance log. The audit request itself appears on the next read.

## HNSW configuration, jobs, and maintenance

`config` is `{m,ef_construction,seed}`; omitted fields default to `16,100,42`.
Bounds: `m` 2–64, `ef_construction` between `m` and 4096. Seed accepts a JSON u64
or canonical decimal u64 string; administrative responses use a string to avoid
JavaScript precision loss. `POST /v1/search` and `POST /v1/index/hnsw` accept this
optional object, so ready-only queries can select a nondefault graph.

`limits` is `{max_records,max_snapshot_vector_bytes}`, default
`100000,134217728`; valid ranges are 1–1000000 and 1–268435456. Limits bound snapshot
vectors/cardinality, **not** total process memory, duration, or graph allocations.
Only `"squared_l2"` and `"cosine"` are ANN metrics.

- `POST /v1/index/hnsw/rebuild` body `{config?,metric?,limits?}` starts one bounded
  job and returns HTTP 202 `{"outcome":"running"}`; a pending job returns 409.
- `GET /v1/index/hnsw/rebuild` observes without polling/publication. It returns the
  same `{policy,status}` as maintenance GET below.
- `POST /v1/index/hnsw/rebuild/poll` with `{}` nonblockingly reaps/publishes a
  completed graph and returns `{outcome}`. Values are `"idle"`, `"running"`,
  `"cancelling"`, `"published"`, `"cancelled"`, `"superseded"`,
  `"catch_up_overflow"`. A terminal result is consumed; subsequent poll is idle.
- `POST /v1/index/hnsw/rebuild/cancel` with `{}` returns `{cancelled:boolean}`.
  Cancellation is cooperative; poll afterward to reap. It preserves the active
  graph and does not wait for the worker.

`GET /v1/index/hnsw/maintenance` returns:

```json
{"policy":{"config":{"m":16,"ef_construction":100,"seed":"42"},"metric":"squared_l2","limits":{"max_records":100000,"max_snapshot_vector_bytes":134217728},"rebuild_at_buffered_ids":128,"rebuild_at_buffer_vector_bytes":4194304},"status":{"ready":false,"pressure":"unavailable","active_buffer":null,"rebuild":null}}
```

`PUT` on this route replaces the process-local policy, validating it before
publication, and returns the same envelope. Omitted fields use defaults (not
previous values). High-water bounds: IDs 1–256 and vector bytes 1–8388608.
Policy resets at restart. **There is no automatic timer or hidden retry.**

`POST /v1/index/hnsw/maintenance/tick` with `{}` explicitly polls/publishes a
matching job, then starts a replacement if necessary. Returns
`{outcome,started:boolean,status}`. Mismatched pending jobs return 409.

`status.pressure` is `"healthy"`, `"rebuild_recommended"`,
`"capacity_exhausted"`, `"unavailable"`, or `"catch_up_overflow"`.
`active_buffer` is null or
`{buffered_ids,buffer_vector_bytes,remaining_ids,remaining_vector_bytes}`.
`rebuild` is null or
`{config,metric,snapshot_records,processed_records,worker_finished,terminal_reason,catchup_buffer}`.
`terminal_reason` is null or an outcome string; `catchup_buffer` uses the same
capacity schema. Progress is record count, not an ETA. Status readiness is for
the selected **maintenance policy's** config/metric. GET never publishes.
Synchronous index POST remains compatible and returns 204. A graph/config change
can require changing search config, including ready-only searches.

## Safe backup and restart-selected restore (admin only)

- `GET /v1/admin/backups` returns
  `{enabled,items:[{name,kind:"backup"|"restore",bytes}],restart_required:true}`.
  At most 1000 generated regular-file entries are listed, without arbitrary paths.
- `POST /v1/admin/backups` with `{}` takes a serialized snapshot while the database
  owner lock excludes writers. It syncs and validates a staged complete copy,
  then publishes without overwrite under a generated `backup-*.vlite` name.
  Response HTTP 201: `{name,inspection,restart_required:true}`.
  Snapshot copying is synchronous database work and may delay reads/writes;
  monitor disk space and schedule large copies during a maintenance window.
- `POST /v1/admin/restores` body `{"backup":"backup-....vlite"}` validates a
  generated existing backup and creates a **new** generated `restore-*.vlite`
  copy without overwriting anything. Response HTTP 201 uses the same schema.
- `POST /v1/admin/restores/select` body `{"name":"restore-....vlite"}` validates
  the restored copy again, then atomically records its basename in the private
  `next-startup` marker. Response `{name,inspection,restart_required:true}`.

`inspection` contains
`{format_version,dimensions,live_records,file_bytes,valid_bytes,incomplete_tail_bytes}`.
Disabled administration returns 409 for copy/selection operations. Arbitrary
paths, traversal, symlinks, and overwrite destinations are not accepted. Local
administrators must not manipulate snapshots concurrently with the service.
Invalid backups never replace the active database.

**Selection does not switch or replace the live owner.** On the next server
startup with the same `VLITE_ADMIN_DIR`, the binary fully validates and opens the
selected restored copy instead of its positional database argument. A configured
`--create-if-missing` dimension must still match. A bad selection fails startup,
never silently falls back. The original database remains untouched and usable
offline. The marker persists across subsequent restarts; remove `next-startup`
offline to return to the positional database, or select another restored copy.
Do not delete/move a selected restored file. There is no upload/download/delete,
remote path selection, or in-place disaster recovery endpoint.

Copy/selection directory sync is used on Unix; Windows directory-entry
power-loss durability retains the engine's documented limitation. An I/O failure
after publication can leave the named output present: inspect before retrying.
Administration is experimental: protect plain HTTP with a trusted TLS proxy,
retain independent off-host backups, and use least privilege for console tokens.
