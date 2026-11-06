# VLite 0.7.2 upgrade and rollback

VLite 0.7.2 adds an atomic key-value batch and a multi-key read (REST, gRPC, the clients, the shell and the MCP tools), adds key-value operations to the embedded libraries and lets the dashboard sign in while agent memory is enabled.

<!-- source: docs/KEY-VALUE.md, docs/API.md#key-value-entries, docs/GRPC.md#key-value-entries, docs/MCP.md#local-key-value-tools, docs/TOOLS.md, bindings/README.md, docs/CONSOLE.md, docs/SECURITY.md#memory-bound-resources, docs/MEMORY.md -->

It follows [0.7.1](RELEASE-0.7.1.md). The store opens unchanged: there is no new file format, setting or migration, and files written by 0.7.1 open as before. The wire changes only add: two REST routes, two gRPC calls, two MCP tools, two shell commands, one audit event and one capability flag. A file written by 0.7.2 opens in 0.7.1 (see [Rollback](#rollback)). No benchmarks were run for this release, and the [0.7.0 numbers](RELEASE-0.7.0.md#benchmarks) make no claim about it.

## What is new

### Key-value: atomic batch and multi-get

Two operations join get, put, delete and list. A batch writes several keys of one collection, all or nothing, and a multi-get reads several keys in one call. Both need explicit use; single-key operations are unchanged. The [key-value guide](KEY-VALUE.md) has the full rules.

- **Where.** REST `POST /v1/collections/{collection}/kv/batch` (needs `write`) and `POST /v1/collections/{collection}/kv/entries/get` (needs `read`, so the read-only token may call it); gRPC `WriteKvBatch` and `GetKvEntries` on `vlite.v1.Kv`; the local MCP tools `vlite_kv_batch` (listed and callable only with `--writable`) and `vlite_kv_get_many`; the shell commands `kv-batch` and `kv-get-many`; the Rust methods `KvStore::write_batch` and `KvView::get_many`; and the clients: `kv_batch` and `kv_get_many` in Python, `kvBatch` and `kvGetMany` in JavaScript, `KvBatchAsync` and `KvGetManyAsync` in .NET, and `KvBatch` and `KvGetMany` in Go. The Go client had no key-value operations before; it now has all six (get, put, delete, list, multi-get and batch).
  <!-- source: docs/API.md#key-value-entries, docs/GRPC.md#key-value-entries, docs/MCP.md#local-key-value-tools, docs/TOOLS.md, clients/README.md, src/server.rs (permission), src/bin/tool_support/kv_mcp.rs, clients/go/kv.go -->
- **What a batch does.** It holds up to 1,000 puts and deletes, for any namespaces of the collection, and commits them as one frame: after a crash you find all of it or none of it, and a reader sees the state before the batch or the state after it, never part of it. Each operation takes the same `expected` condition as the single call (`any`, `absent` or a revision). The first operation that cannot run fails the whole batch and nothing is written. Every put of one batch gets the same version, the commit's sequence, and each is a valid `expected` revision for its own key. A delete of a key with no live entry succeeds with `deleted: false`; if an expired record of the key is still stored, the delete removes it, still reports `false` and counts as a change. A batch that changes no record commits nothing.
  <!-- source: docs/KEY-VALUE.md, docs/API.md#key-value-entries, src/kv.rs (write_batch), src/kv/batch.rs, src/server/kv.rs -->
- **Rules.** Each `(namespace, key)` may appear once. The values of all puts total at most 4,194,304 decoded bytes. A put in a batch has no `expires_at_ms` (the field is unknown, so 422 `invalid_json`) and, like any put without a deadline, clears the deadline of the entry it replaces; use single puts for entries that need one. The request is checked as a whole before anything runs, and a bound, encoding, size, duplicate-key or `expected` fault is 422 `invalid_input` whose message names the operation.
  <!-- source: docs/KEY-VALUE.md (Batches and multi-get, Limits), src/kv.rs (MAX_KV_BATCH_OPERATIONS, MAX_KV_BATCH_VALUE_BYTES), src/kv/batch.rs (check) -->
- **Errors and quota.** A failed condition is 412 `precondition_failed` (gRPC `ABORTED`). All 16 record IDs of a key taken is 409 `kv_slots_exhausted`, and a delete of an entry that a graph node is attached to is 409 `graph_attached_record` (both `FAILED_PRECONDITION`). A batch that changes a record in a file of format 1 is 409 `upgrade_required`. The record quota is judged once, on the count that the whole batch leaves, and only when the batch writes at least one record, so an empty batch or a batch of deletes is never refused for quota. A 503 `commit_indeterminate` (gRPC `UNKNOWN`) means that all of the batch or none of it was written and nobody can tell which: read the keys back with a multi-get before you decide, and do not retry blindly.
  <!-- source: docs/KEY-VALUE.md (Errors, Batches and multi-get), docs/API.md#key-value-entries, src/server/kv.rs (batch) -->
- **Multi-get.** It names 0 to 1,000 keys (a key may repeat) and answers one element per key, in order, with `null` for a key that is absent or due. One read and one expiry clock serve the call. If the entries would pass 16,777,216 bytes of compact JSON, the call returns the longest prefix that fits and `next_index` names the first key it did not read; ask again for the rest. Calls share no snapshot, so a key can change between two calls.
  <!-- source: docs/KEY-VALUE.md (Batches and multi-get, Limits), docs/API.md#key-value-entries, docs/GRPC.md#key-value-entries -->
- **Audit and metrics.** A batch writes one `kv_batch` audit event, not one per operation. It carries the request ID, principal, actor, collection, outcome and `duration_ns`, plus the counts `puts` and `deletes`, and never a namespace, key or value. A batch refused for slots counts once in `kv_slots_exhausted_total`. See [Key-value permissions](SECURITY.md#key-value-permissions).
  <!-- source: docs/SECURITY.md#key-value-permissions, src/server/kv.rs (audit) -->
- **Finding out whether a server has the routes.** `GET /v1/capabilities` lists both routes in `routes` for every file format and gains `features.kv_batch`, which is true for format 5 and later. Look for the two paths in `routes`, not at the flag: a file before format 5 serves both routes while the flag is false.
  <!-- source: src/server/admin.rs (capabilities), docs/KEY-VALUE.md, docs/API.md#key-value-entries -->

### Key-value in the embedded libraries

The C library, the Python package (`vlite-embedded`), the Node.js package (`@vlite/embedded`) and WebAssembly now carry get, put, delete and list, with the rules of the other surfaces: byte values under namespaced text keys, compare-and-set on versions, an optional absolute expiry (it needs a file of format 5) and ordered prefix listing. They work in an existing collection of one dimension, such as the `default` collection of a file created with one dimension, and never create one. A page of a list holds at most `limit` entries (1 to 10,000, default 100) and 16 MiB of JSON. There are no batch, multi-get or lease operations in the embedded libraries.

- **C library.** Key-value is one more request of `vlite_execute`, `{"op":"kv","request":{"op":"get",...}}`. `vlite_abi_version()` still returns 1 and no symbol is added. A library older than the request answers `invalid_input`; the Python and Node.js wrappers report that as `unsupported`.
- **Python, Node.js and WebAssembly.** `db.kv_get`, `db.kv_put`, `db.kv_delete` and `db.kv_list` (keyword-only arguments, `bytes` and `int`); `db.kvGet`, `db.kvPut`, `db.kvDelete` and `db.kvList` in Node.js (an options object, `Uint8Array` and `bigint`) and in WebAssembly (positional arguments). A key that is absent or due is `kv_not_found` in the C library, Python and Node.js, and `undefined` from WebAssembly's `kvGet`. A read-only handle refuses writes with `read_only`.
  <!-- source: bindings/README.md#key-value, bindings/c/FEATURES.md, bindings/c/vlite.h, bindings/c/src/lib.rs (vlite_abi_version), bindings/python/README.md, bindings/node/README.md, bindings/wasm/README.md, docs/KEY-VALUE.md -->

### gRPC applies the memory reservations of its REST router

`vlite-server` still refuses `--grpc-bind` while a memory binding is configured or reserved records remain (see [Known limits](#known-limits)), so a server that runs with memory serves no gRPC and nothing changes for it. What changed is the library path. The gRPC adapter built after a REST router now adopts that router's memory reservations together with its tokens, credential store and audit log, so a reserved collection or graph answers like a missing one on every RPC. Before, such an adapter ignored the reservations. A program that builds the routers itself must build the REST router first (`configured_admin_router`, then `grpc_router`); `configured_admin_router` refuses the opposite order, and `router` and `authenticated_router` log a warning for it. A generated test compares the two transports for every RPC.
<!-- source: docs/GRPC.md, docs/SECURITY.md#memory-bound-resources, src/server.rs (publish), src/server/published.rs, src/server/grpc.rs (grpc_router), src/server/admin.rs (configured_admin_router), tests/server_guard_differential/grpc_leg.rs -->

### Agent memory

Expiry and forget are logical deletion. A memory whose `expires_at_ms` has passed is hidden from reads and later removed from the graph, but its expiry does not carry over to its evidence record: the record stays in the evidence collection until someone with `raw_access` deletes it, or until it expires on its own if it was given an expiry. Only `forget` deletes the evidence record that only the forgotten memory used. Neither is secure erasure. This documents behavior that 0.7.1 already had; nothing changed in the server. See [Forget](MEMORY.md#forget).
<!-- source: docs/MEMORY.md (expires_at_ms, Forget, Limits and error codes) -->

### Clients and guides

The Python, JavaScript and .NET client guides and the JavaScript typings now say how to settle a mutation after a timeout or a cancellation: look up a `forget` or `consolidate` with `get_receipt` and the same idempotency key, and repeat a `remember` with the same key and body inside the receipt window, because the server refuses a lookup of a `remember` receipt (409 `memory_capability_unavailable`, `details.feature` `remember_receipt_lookup`). They also state that text and vector recall find only memories that hold an evidence record, and that the Go client has no agent-memory methods. Client behavior is unchanged apart from the new key-value methods above.
<!-- source: clients/README.md, clients/python/README.md, clients/javascript/README.md, clients/javascript/index.d.ts, clients/dotnet/VLite.Client/MemoryClient.cs -->

## Fixed

- **The dashboard signs in while agent memory is enabled.** While agent memory is enabled the server answers `GET /metrics` and a few other database-wide routes with 403 `forbidden_scope`, even to the administrator, so that memory data cannot be reached through them. The 0.7.1 dashboard read that refusal as a failed sign-in: a super admin got `HTTP 403: forbidden_scope` and could not enter. The dashboard now reads the refusal as the memory case when the credential could otherwise use the route. Sign-in succeeds, the overview shows the database size as Unavailable with a plain note, and Observability, Health, the Prometheus text, webhooks, automatic maintenance, import status, replication export and graph statistics each say in their own panel that they are off while agent memory is enabled. A 401, a 5xx, a network failure and every other 403 still fail as before. The server, the OpenAPI document and the security model are unchanged: the routes still answer 403. See [Monitor the server](CONSOLE.md#monitor-the-server) and [Memory-bound resources](SECURITY.md#memory-bound-resources).
  <!-- source: src/dashboard/app.js (MEMORY_OFF, memoryOff), src/dashboard/observability.js, src/dashboard/console-tools.js, docs/CONSOLE.md, docs/SECURITY.md#memory-bound-resources, src/server/observability.rs -->
- **The MemoryAI clock hint no longer appears for every refused memory request.** The hint ("Your computer or proxy clock may be ahead of the VLite server") matched the server's generic "memory request field is invalid" text, so every 422 `invalid_input` of a memory request showed it. The server refuses an observation time later than its own clock like any core refusal, with that generic message and no named field, while a refused request field always names itself. The hint now appears only for a `remember` or `consolidate` refused with 422 `invalid_input` that names no field or names `observed_at_ms`, and for the dashboard's own "server time is unavailable" error. Recall, forget, receipt lookups, other statuses and other fields show the server's own message alone.
  <!-- source: src/dashboard/console-tools.js (memoryClockGuidance) -->
- **A vertical touch drag on a graph is handled by the graph.** On a touch screen, a vertical drag that starts on the 2D diagram or the 3D stage moves a node or pans or turns the graph instead of scrolling the page. A drag that starts anywhere else, and the mouse wheel, still scroll the page, and the gesture hint says so.
  <!-- source: src/dashboard/graph.css, src/dashboard/graph.js, docs/CONSOLE.md -->

## Upgrade

Back up and verify checksums first. Replace the binary or image with 0.7.2 and restart. The store opens unchanged: there is no format change, no setting and no data migration, and existing files are never converted in place. Follow the single-writer rule in the same order as for 0.7.1: stop the old process (or deactivate the old revision and wait until it has no replica), then start the new one. The [0.7.1 upgrade section](RELEASE-0.7.1.md#upgrade) explains why and covers hosted apps. If you upgrade from 0.7.0, the 0.7.1 changes come with it: read its checks and [First start under 0.7.1](RELEASE-0.7.1.md#first-start-under-071) first. 0.7.2 adds no check of that kind.
<!-- source: docs/RELEASE-0.7.1.md#upgrade, docs/KEY-VALUE.md, docs/API.md#key-value-entries -->

Check these before you upgrade:

- **Servers before clients.** A 0.7.2 client that calls a batch or a multi-get on a 0.7.1 server gets 404 `not_found` with the administrator token (the clients raise their unsupported-operation error) and 403 with every other credential (an ordinary permission error), because that server checks permissions before it matches a route. Over gRPC the 0.7.1 server answers `UNIMPLEMENTED` to every caller. Nothing falls back to single calls. To find out beforehand, look for the two paths in `routes` of `GET /v1/capabilities`; gRPC has no capabilities call, so a gRPC client asks REST.
- **Audit queries.** A query, alert or export that counts key-value writes by the `kv_put` and `kv_delete` events must also count `kv_batch`, which is one event per batch with `puts` and `deletes` counts.
- **Group commit.** If you run `--group-commit` and send large batches, keep `--group-commit-bytes` at 8 MiB or more. Each mutating request is charged an estimate of the memory it retains against that budget (default 64 MiB), and a request that alone needs more than the whole budget is refused with 503 `busy` on every attempt. A batch at every limit at once is charged less than 8 MiB.
- **gRPC clients.** The proto file only gains messages and RPCs, so existing stubs keep working. Generate new ones to call `WriteKvBatch` and `GetKvEntries`, and give the channel a receive limit of at least 16 MiB + 1 KiB for `ListKvEntries` and `GetKvEntries` and a send limit above 6 MiB for a full `WriteKvBatch`.
- **Programs that build the routers themselves.** Build the REST router first and the gRPC router after it.
  <!-- source: docs/KEY-VALUE.md (Batches and multi-get, Limits), docs/GRPC.md (Older servers), docs/SECURITY.md#audit-actors, src/server/admin.rs -->

These changes apply automatically and need no action: the dashboard fixes and the reservation parity of the gRPC library path. The batch, the multi-get and the embedded key-value operations need explicit use.

## Rollback

Restart with the 0.7.1 binary or image, in the same order as the upgrade: stop the 0.7.2 process (or deactivate its revision and wait until it has no replica), then start 0.7.1 from its pinned image digest. 0.7.2 writes nothing that 0.7.1 cannot read, and there is no exception. A batch commits the ordinary atomic batch frame that every build since format 2 reads, so each entry it wrote is an ordinary record that 0.7.1 reads, lists, updates and deletes. Nothing about a batch is one-way.
<!-- source: src/kv.rs (write_batch, commit), src/lib.rs (write_batch_in), docs/KEY-VALUE.md (Batches and multi-get), docs/projects/roadmap/kv-batch-contract.md (2.6 Compatibility and rollback) -->

After a rollback:

- **Batch and multi-get stop working.** 0.7.1 has neither route, RPC, MCP tool nor shell command. A call over REST answers 404 `not_found` to the administrator token and 403 to every other credential, and a gRPC call answers `UNIMPLEMENTED` to every caller. Stop clients and scripts from sending them.
- **The embedded key-value methods go with the packages.** The 0.7.1 libraries have no key-value request. Entries written through them stay in the file, and the 0.7.1 server, shell and MCP tools read them.
- **Audit log.** The log is only appended to, so the `kv_batch` lines stay. 0.7.1 reads each line of the log as JSON without a fixed list of event names, so it reads a log that holds `kv_batch` lines without error.
- **The dashboard fixes go.** With agent memory enabled, the 0.7.1 dashboard again fails sign-in for a super admin with `HTTP 403: forbidden_scope`, the MemoryAI clock hint again appears for every 422 `invalid_input` of a memory request, and a vertical touch drag over a graph again scrolls the page.
- **Further back.** Rolling back past 0.7.1 to 0.7.0 has its own exceptions, which the [0.7.1 rollback section](RELEASE-0.7.1.md#rollback) lists: item evidence references and the gRPC disk-ANN path rule.
  <!-- source: src/server/audit.rs (tail), docs/KEY-VALUE.md, docs/GRPC.md (Older servers), docs/API.md#key-value-entries, bindings/c/FEATURES.md, docs/RELEASE-0.7.1.md#rollback, src/dashboard/app.js -->

## Compatibility

- **Files and settings.** No file format, flag or setting changes, and no server dependency changes: `Cargo.toml` and `Cargo.lock` match 0.7.1 apart from the version, and the minimum Rust version (1.89) is unchanged. The WebAssembly package adds one dependency, `js-sys` (default features off), for its key-value methods.
  <!-- source: Cargo.toml, Cargo.lock, bindings/wasm/Cargo.toml, docs/RELEASE-0.7.1.md#compatibility -->
- **Wire.** The OpenAPI document gains the two paths and their schemas, and the proto file gains the two RPCs `GetKvEntries` and `WriteKvBatch` and ten messages; nothing existing is removed or changed. `GET /v1/capabilities` gains `features.kv_batch` and the two paths in `routes`. The OpenAPI `info.version` reads 0.7.2.
  <!-- source: src/openapi.json, proto/vlite/v1/vlite.proto, docs/GRPC.md#key-value-entries, src/server/admin.rs -->
- **Audit log.** The new `kv_batch` event is described above. The existing events keep their fields.
  <!-- source: docs/SECURITY.md#audit-actors, src/server/kv.rs -->
- **Tools.** `vlite-mcp` lists `vlite_kv_get_many` always and `vlite_kv_batch` only with `--writable`; a read-only session refuses the batch with `read_only`. `vlite-shell` gains `kv-get-many` and `kv-batch`, and `--read-only` refuses `kv-batch`. Their request fields and bounds are those of the REST routes.
  <!-- source: docs/MCP.md#local-key-value-tools, docs/TOOLS.md#local-key-value-shell-commands, src/bin/tool_support/kv_mcp.rs -->
- **Rust.** `KvOp` and `KvOutcome` (both `#[non_exhaustive]`), the constants `MAX_KV_BATCH_OPERATIONS`, `MAX_KV_BATCH_VALUE_BYTES` and `MAX_KV_GET_MANY_KEYS`, `KvStore::write_batch` and `write_batch_at`, and `KvView::get_many`, `get_many_at` and `visit_many_at` are new public items in `vlite::kv`. Existing items are unchanged.
  <!-- source: src/kv.rs, tests/fixtures/api_surface/vlite-default.txt, site/api.html -->
- **Embedded libraries.** The C ABI stays at version 1 with no new symbol, and `vlite.h` gains only a comment. The Python, Node.js and WebAssembly packages gain the four key-value methods.
  <!-- source: bindings/c/vlite.h, bindings/c/src/lib.rs, bindings/README.md#key-value -->
- **Clients.** The Python, JavaScript, .NET and Go clients gain the batch and the multi-get; the Go client gains key-value as a whole. Each client checks the bounds before it sends, never retries and never falls back to single calls.
  <!-- source: clients/README.md -->
- **Memory.** The first-slice refusals and error codes are unchanged; every refusal and its `details.feature` is in [Agent memory (REST)](API.md#first-slice-limits).

## Known limits

- **Server-wide metrics while agent memory is enabled.** While a memory bindings file is configured, or the database still holds memory binding records after the file is removed, `GET /metrics` (JSON and Prometheus text), `GET /v1/admin/metrics/summary`, graph guard statistics, replication export, webhook list and status, every import route and the automatic-maintenance routes answer 403 `forbidden_scope` to every credential, the administrator token included. This did not change. What changed is that the dashboard shows these views as unavailable instead of failing sign-in, so with memory on the overview, Observability and Health stay empty of server metrics.
  <!-- source: docs/SECURITY.md#memory-bound-resources, src/server/observability.rs (metrics_route, summary), src/dashboard/app.js -->
- **No gRPC and no automatic index maintenance with memory.** `vlite-server` refuses to start with `--grpc-bind` or with automatic index maintenance while a memory binding is configured or the database holds reserved memory resources ("gRPC listener is disabled while memory bindings are configured", or "...while the database holds reserved memory resources"). The gRPC adapter applies the reservations, but the refusal stays.
  <!-- source: src/server/admin.rs (configured_admin_router), docs/GRPC.md, docs/SECURITY.md#memory-bound-resources -->
- **Backups are the administrator's while anything is reserved.** A backup job that uses a backup credential gets 403 once memory bindings exist: run it with the administrator token, or take the backups before you enable memory.
  <!-- source: src/server.rs (authorize_need), docs/SECURITY.md#memory-bound-resources -->
- **Batch.** One collection per batch, at most 1,000 operations and 4,194,304 value bytes, each key once, no deadline on a batch put and no check-only operation. Over REST, a batch inside every other limit can still pass the 8 MiB body limit when many of its keys are escaped in JSON, and then answers 413 `invalid_json`; a batch is atomic and cannot be split, so use fewer or shorter keys. A batch that changes a record in a file of format 1 is refused.
- **Multi-get.** At most 1,000 keys per call, and a long result stops early with `next_index`; the client never continues it, and calls share no snapshot.
  <!-- source: docs/KEY-VALUE.md (Batches and multi-get, Limits, Choose a surface) -->
- **Key-value generally.** A list still reads the whole collection, so its cost grows with the number of entries; keep large key spaces in their own collections and list by a narrow prefix. A version is a compare-and-set token, not a fencing token. The local shell and MCP tools, `kv-batch` and `vlite_kv_batch` included, open the file themselves, so stop `vlite-server` first; `vlite-mcp --server` does not forward them.
  <!-- source: docs/KEY-VALUE.md (Listing, paging and its cost; Revisions are not fencing tokens) -->
- **A credential that can write but not read can still probe a key it may overwrite.** It can learn whether the key is live (a delete with `expected=absent` answers 412 for a live key and `deleted: false` for a missing one, and changes no live entry either way), and it can test a guessed revision when it can make the request fail in some other way after the condition. A batch widens that class, because it can add a failing operation on another key. It never learns a value. The 0.7.1 guide said that such a credential cannot learn whether a key exists; the guides now say what holds. Give `write` on a key-value collection only to principals you trust with its keys.
  <!-- source: docs/SECURITY.md#key-value-permissions, docs/API.md#key-value-entries, docs/KEY-VALUE.md (Compare-and-set with revisions) -->
- **Embedded key-value.** The embedded libraries open the file inside your process, so the single-writer rule holds; they have the four single-key operations only.
  <!-- source: bindings/README.md#key-value, docs/KEY-VALUE.md (Choose a surface) -->
- **Deletion in agent memory is logical.** Forget and expiry are not secure erasure, and an expired memory's evidence record stays until it is deleted or expires on its own. Plan backup retention and external copies yourself.
  <!-- source: docs/MEMORY.md -->

## Not included in this release

These are not available, and you should not rely on them.
<!-- source: docs/RELEASE-0.7.1.md#not-included-in-this-release, docs/API.md#first-slice-limits, docs/RELEASE-0.7.0.md#not-included-in-this-release -->

- **Compact and float16 vector storage and compact graph storage.** Float32 stays the only storage precision and production graph storage is unchanged.
  <!-- source: docs/projects/performance/storage-goals.md -->
- **Agent-memory facts and erasure jobs.** `facts/query`, `facts/correct`, `erasures/get` and `erasures/resume` answer 409 `memory_capability_unavailable` with `details.feature`, and `forget` by series, entity, session or scope is refused the same way. `forget` by ID is logical deletion, not secure erasure.
- **Approximate recall.** `mode: approximate` answers 422 `memory_bounded_plan_unsupported`: no approximate index is built for a binding, and none is built lazily.
- **Agent memory outside REST.** Memory is served over REST and by the clients, the command-line client and the MCP tools that call it. There is no gRPC memory service and no embedded memory API, and an operator provisions every binding in the bindings file: no route or dashboard control creates one.
  <!-- source: proto/vlite/v1/vlite.proto (services VLite and Kv), src/lib.rs (mod memory), src/openapi.json (memory routes), docs/MEMORY.md -->
- **Leases and a fencing-token ledger.** These are not implemented. Key-value versions are not such a ledger (see [Key-value store](KEY-VALUE.md#revisions-are-not-fencing-tokens)).
  <!-- source: docs/projects/roadmap/gap2-lease-ledger.md, docs/STORAGE-FEATURES.md#key-value-store -->
- **Unchanged from 0.7.0:** temporal graph, a Cypher-style query subset and document-to-graph ingestion are not available (see [0.7.0](RELEASE-0.7.0.md#not-included-in-this-release)).
