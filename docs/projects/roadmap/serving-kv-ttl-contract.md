# Serving contract: namespaced KV, fenced leases and graph TTL

Status: proposed exact schema for owner review; not implemented or approved by
this document. The goals are approved and exact schemas are reviewed before
code (`docs/projects/roadmap/next-major.md:5,18-19,91-94,106-112` and
`:49-55,239-241`); a storage-format change arrives only with its own converter
(`:237-238`).
This document is conformed to the [serving wire conventions proposal](serving-wire-conventions.md), whose owner decisions remain open.
Scope: product Gaps 2 and 3. Normative words below describe proposed behavior;
only the evidence table describes Current behavior. Cross-document wire choices
(proto tags, error codes, the `GraphChangeOperation` amendment, TTL-operation
placement) follow that proposal pending owner ratification. No
benchmark or capacity claim is made, and no default, gate, policy or release
criterion changes. `src/openapi.json` and `proto/vlite/v1/vlite.proto` stay
frozen until the owner approves this document.

Non-goals: distributed consensus, cross-database transactions, automatic
fencing of external services, temporal/as-of APIs, record-expiry changes,
arbitrary KV batches, and implementing anything in this documentation slice.
Related: [serving model](serving-contract.md),
[roadmap](next-major.md), [product gaps](../product-gaps/product-gaps.md),
[storage](../../STORAGE-FEATURES.md#key-value-store),
[graph expiry](../../GRAPH.md#expiry-ttl),
[attachment decisions](../feature-agent-graph/decisions.md).

## 1. Testable requirements

1. **KV-001:** all adapters implement the same namespace/key validation,
   prefix ordering, expiry visibility and atomic expected-version checks.
2. **KV-002:** a KV write acknowledges only after the handle's commit
   completes, and that commit is a sync only under `--durability full`
   (`periodic`, the default, can lose up to one flush interval on OS crash or
   power loss; `none` keeps nothing across restart). Lease-ledger writes always
   sync or the lease route refuses (§5). Indeterminate outcomes are never
   retried automatically.
3. **LEASE-001:** successful acquisitions return strictly increasing,
   never-reused fencing tokens in one durable fence domain, including after
   crashes, normal restarts, collection recreation and maintenance; where a
   restore, promotion or recovery could reinstate an older ledger, acquisition
   fails closed (§5).
4. **LEASE-002:** stale, expired, revoked and pre-restart holders cannot renew
   or revoke a newer lease. No adapter substitutes a record revision for a
   lease fencing token.
5. **TTL-001:** graph deadline visibility and batch behavior match the embedded
   engine at the inclusive `deadline <= now_ms` boundary.
6. **TTL-002:** every served node/edge projection reports its own deadline;
   clearing a live entity's deadline bumps its revision atomically.
7. **FEED-001:** an opt-in TTL-aware feed reports deadline changes and
   committed removals in deterministic replay order, without changing the
   default feed or existing event types.
8. **AUTH-001:** authorize tenant, resource binding and operation before
   lookup; identical REST, gRPC and MCP requests have identical outcomes.
9. **ATTACH-001:** a vector record attached to a graph node never acquires
   expiry; all adapters surface `graph_attached_record_expiry`.
10. **SHIP-001:** schemas, client types, reference rows, goldens, independent
    oracle tests and independent acceptance land together per delivery slice.

## 2. Current coverage and verified evidence

No means absent from the inspected dispatch/schema, not a claim about every
possible integration. Evidence line numbers refer to this checkout.

| Capability | Rust engine | REST / gRPC | MCP / CLI / bindings |
|---|---|---|---|
| KV get, put/CAS, delete, ordered prefix list | Yes [K1, K2] | No KV route/RPC in inspected registrations [S1, P1] | Proposed here; no current-support claim |
| KV expiry | v5 required; due keys are absent [K1, K3] | No KV schema [S1, P1] | Proposed here |
| KV batch and acquire/renew/revoke | No such public methods in `KvView`/`KvStore`; future Rust work (§12) [K2] | Proposed leases only; no batch | Proposed here |
| Durability of an acknowledged write | `Full` syncs per commit; the default `Periodic` and `None` do not [D1] | Server `--durability`; optional `--group-commit` [D1] | Not a per-call option |
| Versions as fences | Revisions within one file lineage; rewrites invalidate them [K1] | Not a durable lease-token allocator | Must not promise more than [K1] |
| Graph TTL set/clear and reads | Yes [T1, T2] | REST batch/parser and encoder omit TTL; gRPC has no graph mutation RPC [S2, P1] | Shared tool encoder/parser omits TTL [S2] |
| Graph feed deadline/removal | Entries are currently skipped [F2] | Existing graph batches only [F1, P2] | TTL feed proposed here |
| Attached-record expiry protection | Enforced [A1] | Named graph conflict already mapped [A2] | Shared tool error name exists [A2] |

- **K1:** `src/kv.rs:1-19` documents revisions, rewrite caveats and v5;
  `src/kv.rs:26-39` defines `KV_PROBE_SLOTS=16`,
  `MAX_KV_NAMESPACE_BYTES=255`, `MAX_KV_KEY_BYTES=1024`,
  `MAX_KV_VALUE_BYTES=MAX_PAYLOAD_BYTES-HEADER_BYTES(7)-255-1024`,
  `MAX_KV_LIST_LIMIT=10000`; `src/lib.rs:94` defines
  `MAX_PAYLOAD_BYTES=1048576`, hence value bound **1047290 decoded bytes**.
- **K2:** `src/kv.rs:84-113` opens a one-dimension collection (`kv_store`
  creates it implicitly, `kv_view` requires it);
  `src/kv.rs:129-171` exposes `KvView` get/list and injected-clock variants;
  `src/kv.rs:173-265` exposes `KvStore` put/delete with `Expected`, but no
  batch or lease lifecycle method, and `src/lib.rs:32` only declares the
  module. A search of `src/` for lease or batch KV code finds nothing (the
  unrelated reader guard `SnapshotLease` is at `src/records.rs:219`). A put
  without expiry clears it (`src/kv.rs:187-234`, `src/format.rs:389-394`).
  Entries are ordinary one-dimension records (`src/kv.rs:3-6,115-121`), so any
  collection with one dimension passes `kv_view`/`kv_store`.
- **K3:** `src/kv.rs:288-322` probes all slots and compares full identities;
  `src/kv.rs:352-388` filters due entries, sorts keys and uses exclusive
  `after`; `src/kv.rs:390-403` validates nonempty UTF-8 byte lengths.
- **D1:** `src/durability.rs:15-30` defines the default `Periodic` (write to
  the OS before the ack, background sync, up to about one interval lost on OS
  crash), `Full` and memory-only `None`; `src/lib.rs:2265-2276` syncs each
  commit only without a flusher and `src/lib.rs:2321-2328` leaves group frames
  to the flusher under `Periodic`; `Database::sync` is a no-op under `Full`,
  read-only and `None` (`src/lib.rs:1688-1692`); server flags
  `--durability` and `--group-commit` (`src/bin/vlite-server.rs:18`);
  `docs/STORAGE-FEATURES.md:241-253`. `src/kv.rs:185-186` says "Durably
  writes" and means this handle mode.
- **T1:** `src/property_graph/mod.rs:149-177` defines the read clock;
  `src/lib.rs:100-107` uses server `SystemTime` since Unix epoch.
  `src/property_graph/engine/state.rs:364-379` implements inclusive expiry
  and endpoint cascade. `src/property_graph/mod.rs:281-292,510-535` exposes
  set/clear; `src/property_graph/read.rs:195,212` carries deadlines.
- **T2:** `docs/GRAPH.md:64-94` specifies revision bumps, one write clock,
  atomic reclaim, non-resurrection, maintenance and unchanged operation cap;
  `src/property_graph/engine/mutate.rs:260,316` excludes computed reclaim
  entries from the ordinary operation count.
- **S1:** `src/server/features.rs:10-50` and
  `src/server/graph.rs:123-184` register feature/graph routes;
  `src/server.rs:742` applies the body limit.
- **S2:** `src/bin/tool_support/graph_json.rs:208-305` lists input operations;
  `src/bin/tool_support/graph_json.rs:731-753` omits deadline output.
  REST shares these operations (`src/server/graph.rs:700-730`).
  CLI commands and atomic batch shape: `src/bin/vlite-graph-cli.rs:33-82`.
- **F1:** `src/changes.rs:501-583` serializes existing graph operations;
  the node-patch arm around **535** has no expiry field.
  `src/server/features.rs:63-127` wraps events and parses cursor/limit.
- **F2:** `src/property_graph/feed.rs:104-137` decodes graph-batch vector
  mutations to `Mutation::Put`/`PutWithMetadata`/`Delete`, which carry no
  expiry, so `ChangeKind::Put.expires_at_ms=None` at `:118,132` says nothing
  about graph deadlines. The missing graph feed support is
  `src/property_graph/feed.rs:270-277`, which skips `NodeExpiry`, `EdgeExpiry`
  and `Reclaim`.
- **P1:** `proto/vlite/v1/vlite.proto:16-60` lists current RPCs;
  `proto/vlite/v1/vlite.proto:11` documents request/response transport caps.
- **P2:** `proto/vlite/v1/vlite.proto:455-524` defines changes requests,
  `GraphChangeOp` values 0-9 and `GraphChangeOperation` fields 1-18.
- **A1:** `src/lib.rs:1994-1996` rejects expiry on an attached record;
  attachment invariants and H-4/LEG-004 are recorded in
  `docs/projects/feature-agent-graph/decisions.md:279-281,318,553` and
  `docs/projects/feature-agent-graph/feature-agent-graph.prd.md:269,303,372`.
  Gap 3 supersedes REQ-014's original node/edge deferral, not its attached
  record prohibition.
- **A2:** `src/server.rs:1611,1648` maps the conflict and stable code;
  `src/bin/tool_support/graph_json.rs:64` uses the same code.
- **AUTH:** `src/server/credentials.rs:25-59` enumerates existing scopes;
  `src/server/graph.rs:11-54,85-98` specifies graph bindings, tenant naming,
  attachment authorization and feed filtering.
- **WIRE:** `src/server.rs:60,742,1407-1419,1523-1527,3474-3503` supplies the
  8 MiB body cap, error envelope, precondition status and credential rate
  buckets; `src/server.rs:1471-1478,1545-1565,1658-1662` maps oversize
  payloads to 422 `invalid_input`, the `cursor_*` family and extractor
  failures to `invalid_json`; `src/server/grpc.rs:23-25,98-99,181-216`
  supplies the 8 MiB request/16 MiB response caps and status/trailer mappings
  (every other 4xx becomes `INVALID_ARGUMENT`, `:194`);
  `src/server/grpc.rs:309-316,378-385` treats an empty `collection` as the
  legacy `default` and an unset `Expected` as `any`.

## 3. Shared wire types and bounds

All new JSON objects reject duplicate/unknown fields on input; output readers
ignore unknown fields. `D` is a canonical decimal string representing
`0..18446744073709551615`, no sign or leading zero except `"0"`.
Issued versions and fencing tokens are nonzero; expected revision zero is
valid input that cannot match a live version. Graph IDs may be zero.
`Deadline = D | null`; timestamps are absolute Unix milliseconds,
not seconds. JSON integers are not accepted for new KV fields. Existing graph
ID/revision integer input remains accepted, without changing its old schema.
JSON null never means zero. All strings are valid Unicode scalar sequences,
measured in UTF-8 bytes, without normalization or case folding. Bodies that
fail these rules (syntax, unknown or duplicate fields, wrong types) use the
existing extractor mapping, 400 syntax / 422 data, both `invalid_json`
(`src/server.rs:1659-1662`); values that parse but break a bound are 422
`invalid_input`.

| Type | Exact fields / constraints |
|---|---|
| `KvAddress` | `namespace:string` 1..255 bytes, `key:string` 1..1024 bytes |
| `Expected` | `"any"` (default when omitted), `"absent"`, or `{"revision":D}`; exactly one alternative. gRPC reuses `vlite.v1.Expected`, whose unset oneof means `any` (`proto/vlite/v1/vlite.proto:112`, `src/server/grpc.rs:378-385`) |
| `KvPut` | Address + required `value_base64:string`; optional `expected:Expected="any"`, `expires_at_ms:Deadline=null` |
| `KvDelete` | Address + optional `expected:Expected="any"` (REST sends the fields as query parameters, §4; gRPC as a message) |
| `KvEntry` | required `key:string,value_base64:string,version:D,expires_at_ms:Deadline` |
| `KvPutResult` | required `version:D,expires_at_ms:Deadline` |
| `KvDeleteResult` | required `deleted:boolean` |
| `KvPage` | required `items:KvEntry[],next_after:string|null` |
| `LeaseAcquire` | Address + required `ttl_ms:integer` 1..86400000 |
| `LeaseRenew` | Address + required `fence_domain:string`, `fencing_token:D` nonzero, `ttl_ms:integer` 1..86400000 |
| `LeaseRevoke` | Address + required `fence_domain:string`, `fencing_token:D` nonzero |
| `Lease` | required `namespace:string,key:string,fence_domain:string,fencing_token:D,expires_at_ms:D` |
| `LeaseRevoked` | required `revoked:boolean` (always true on success) |

`fence_domain` is a lowercase UUID string (random, generated with `getrandom`,
already a dependency at `Cargo.toml:59`) identifying a durable database-wide
allocator, not a collection, principal, process or record revision.
Values are padded RFC 4648 standard base64, canonical encoding only, at most
`MAX_KV_VALUE_BYTES` = 1047290 decoded bytes / 1396388 encoded characters
(`src/kv.rs:26-39`). Empty values are valid.
No key/namespace enumeration endpoint is added. Namespace is an application
partition inside an authorized collection, **not an authorization boundary**
(§10). KV entries are ordinary one-dimensional records [K2]: vector, graph
attach and import routes can read, overwrite, delete or expire them, and they
share the collection's quota and probe slots, so KV belongs in dedicated
collections (§4). The engine's 16 collision-probe slots are retained;
exhaustion is an explicit conflict, never a hash-only overwrite [K1, K3].

List `prefix` defaults to `""` (0..1024 bytes); `after` is omitted or a
nonempty key (at most 1024 bytes); `limit` defaults to 100 and is 1..10000
(`MAX_KV_LIST_LIMIT`), else 422 `invalid_input`. Prefix/after length checks are
serving-layer additions; the engine validates only namespace and limit
(`src/kv.rs:361-365`). Keys compare by unsigned UTF-8 bytes, ascending; `after`
is exclusive (`src/kv.rs:376-382`). No snapshot spans pages: an insertion before
the cursor can be missed, and deleting entries does not invalidate it. One page
uses one read guard and one TTL clock. A page holds the largest prefix of
results that fits both `limit` and the response cap below; `next_after` is the
last returned key if another matching live entry existed at that page's clock,
otherwise null; an empty page never carries a continuation. gRPC applies the
same boundary, computed from the canonical JSON form.

Existing transport ceilings apply unchanged [P1, WIRE]: REST bodies are capped
at 8 MiB (`MAX_JSON_BYTES`, `src/server.rs:60`; an oversize body is 413
`invalid_json`, decision (h)); gRPC requests at 8 MiB and responses at 16 MiB
(`src/server/grpc.rs:23-25`; tonic itself answers `OUT_OF_RANGE`, with no
`vlite-error-code` trailer: before the handler for a request, after it for a
response). The largest `KvPut` carries a
1396388-character value, inside both caps, so no KV-specific size code is
added. Out-of-range input is rejected, never truncated or clamped.

## 4. Gap 2 REST contract

`C = /v1/collections/{collection}`; collection is a tenant-local collection
name, subject to existing collection-name validation. Query values use UTF-8
percent encoding, decoded once; key/namespace are never path segments.
No legacy-default KV route is added, and an empty gRPC `collection` is
`invalid_input` for KV instead of the legacy `default`
(`src/server/grpc.rs:309-316`). All successes are HTTP 200 with a JSON body,
including insert, missing delete and lease acquisition: a CAS caller needs the
new `version` that vector PUT's 204 + `ETag` convention does not return
(`src/openapi.json:57,115`), and DELETE keeps the existing 200 `{"deleted"}`
shape (`src/openapi.json:115`), taking its address and `expected` as query
parameters, not a body (decision (f)).

| Method and path | Request | Success body | operationId / scope |
|---|---|---|---|
| GET `C/kv/entry` | required query `namespace,key`; no body | `KvEntry`; absent/due -> 404 `kv_not_found` | `KvGet` / read |
| PUT `C/kv/entry` | `KvPut` body; no query | `KvPutResult` | `KvPut` / write |
| DELETE `C/kv/entry` | required query `namespace,key`; optional `expected=any\|absent\|<D>` (default `any`); no body | `KvDeleteResult` | `KvDelete` / write |
| GET `C/kv/entries` | required `namespace`; optional `prefix,after,limit`; no body | `KvPage` | `KvList` / read |
| POST `C/kv/leases/acquire` | `LeaseAcquire` | `Lease` | `KvLeaseAcquire` / write |
| POST `C/kv/leases/renew` | `LeaseRenew` | `Lease` | `KvLeaseRenew` / write |
| POST `C/kv/leases/revoke` | `LeaseRevoke` | `LeaseRevoked` | `KvLeaseRevoke` / write |

Unlisted methods receive 405; unknown routes receive 404. No lease-get,
KV-batch, or separate CAS endpoint/RPC/tool is proposed: Rust has no KV batch
method and no lease lifecycle API [K2], so both are future Rust work (§12).
CAS is `PUT` with `expected:{"revision":"42"}`; create-if-absent is
`expected:"absent"`; deletes check `expected` atomically. Absent/due keys match
`absent`; revision matching requires a live key; a mismatch is 412
`precondition_failed` (`src/server.rs:1523-1527`). Put omission/null clears
TTL, unlike graph patch/upsert which preserves TTL. An already-due put is
allowed, returns a version and is immediately absent. A TTL put on a pre-v5
store is 409 `upgrade_required` (`src/server.rs:1481-1485`); there is no
automatic file upgrade. Collections must already exist and be one-dimensional:
unlike Rust `kv_store`, serving never creates one implicitly [K2] and relies on
the separately authorized collection-creation API. A collection that is not
one-dimensional is 409 `not_kv_collection`; because entries are ordinary
records (§3), operators dedicate collections to KV.

Ordinary KV operations are linearizable within the exclusively owned
database: checks, writes and expiry decisions share the owner guard.
Reads do not promise replica freshness; lease mutation is primary-only.
Acknowledgement and durability follow KV-002 [D1]. A disconnect, timeout or
`commit_indeterminate` can hide a committed write. No automatic retry, server
request-id deduplication, or exactly-once promise is added. Unconditional put
repeats create new versions, even for identical bytes. Delete-any is
state-idempotent; repeated CAS can fail after first success. KV versions retain
[K1]'s rewrite caveats; clients re-read after maintenance.

## 5. Lease state machine and durable fencing

Leases occupy a **separate logical resource space** from KV values, indexed
by physical collection identity, namespace and key. Acquiring one never
overwrites a KV value. Ordinary KV CAS is not implicitly protected by a
lease; external/protected resources must explicitly enforce fencing.
The authenticated holder is the acquiring credential/account identity
(stable across its token reload), or the configured admin identity. Renew
and revoke require the same holder plus the current domain and token; scopes alone
do not permit takeover. Revoking another holder is not an admin override
in these endpoints. Credential revocation prevents further use.

| Operation / condition under one owner write guard | Result |
|---|---|
| Acquire with no live grant | Allocate token greater than the ledger high-water; persist grant, holder, domain, high-water and deadline with a sync; return `Lease` |
| Acquire while live | 409 `lease_held`, no token allocation or holder disclosure |
| Renew by current holder/token while `now < deadline` | Keep token; replace deadline with `now + ttl_ms`; synced |
| Renew at/after deadline, after revoke/restart, or with another domain/token/holder | 409 `stale_fencing_token`; never resurrect |
| Revoke current live holder/token | Mark inactive with a sync; return `{"revoked":true}` |
| Revoke expired/inactive/foreign holder/token | 409 `stale_fencing_token`; cannot revoke successor |
| Expire | At `now >= deadline` the grant is inactive; next acquire may replace it with a strictly larger token |

Lease `now` is `max(server Unix wall time, last lease clock in this process)`;
clock rollback cannot extend an already-observed deadline. Addition overflow
is 422 `invalid_input`. Restart **invalidates all outstanding leases** before
any request is admitted: a read-write open writes and syncs a recovery record
in the ledger, and an open that cannot (read-only, replica, shared) refuses
lease routes with 409 `lease_unavailable`, as does `--durability none`
(decision (a)). A restarted holder must acquire again and receives a larger
token. Renew is not retry-idempotent (it can extend again); acquire and revoke
are not retry-idempotent either. Clients must treat ambiguous lease outcomes as
lost ownership and reacquire after the possibly-live grant expires; never
invent or infer a token.

**Storage prerequisite (future Rust work, §12; not Current support).** The
engine has no lease lifecycle or allocator today [K2], and a token that must
never repeat cannot live in a record revision, which a rewrite or recreate
resets [K1]. The ledger is therefore a **storage-format change**: an immutable
`fence_domain`, an unsigned high-water, per-key grant state (holder, token,
deadline, active flag) and recovery state, in a new file section behind a
capability flag so an older reader refuses the file instead of ignoring it
(precedent: `docs/GRAPH.md:88-92`). Format changes need P2's converter and
reviewed schemas before code (`docs/projects/roadmap/next-major.md:49-55,237-241`),
so the owner approves the ledger layout first (decision (b)). Allocation is
persisted and synced before a grant is published: a crash may burn tokens but
never reuses one. Every lease write syncs in any durability mode or the route
refuses (decision (a)); a failed sync is the existing 503 `commit_indeterminate`
and poisons lease serving (`src/server.rs:1577-1593`). The ledger survives
compaction (`src/maintenance.rs:460`), upgrade (`src/maintenance.rs:545`),
re-encryption (`src/logops.rs:25`), `convert_to_directory`
(`src/maintenance.rs:219`), directory checkpoints
(`docs/projects/roadmap/p2-directory-store.md:130-136`), collection
drop/recreate and restart, and no record, KV, import or drop path may write it.
Do not approximate it with ordinary KV writes. The future Rust lease types must
not reuse the existing `SnapshotLease` name (`src/records.rs:219`).

A restore, promotion or copy can reinstate an older ledger, which cannot prove a
globally new high-water. Lease routes therefore fail closed with 409
`fencing_recovery_required` after backup restore (`src/maintenance.rs:397`),
point-in-time restore (`src/logops.rs:99`; new lineage, `docs/STORAGE-FEATURES.md:69`),
directory-checkpoint restore, branch/copy, rollback and **replica promotion**
(`src/replica.rs:194-197`: the old primary must not be written with the same
lineage; `docs/projects/roadmap/scale-out.md:28`: promotion has no fencing
protocol), unless an external non-rollback authority proves a floor above every
token ever issued in that domain (`scale-out.md:357`). Replication lag means a
promoted replica can hold an older high-water even if ledger writes ship in
replica logs, so promotion always fails closed. `fence_domain`, not the
change-feed lineage (`src/changes.rs:3-6`), binds the ledger. Recommended first
release (decision (c)): no such recovery; require a fresh domain and explicit
downstream re-enrollment. New-domain tokens are not comparable to old-domain
tokens. A resource must bind one domain and reject another until an
administrator re-enrolls it.

Downstream protocol: atomically compare the presented `(domain,token)` with
its accepted domain and durable highest token; reject smaller tokens, accept
equal tokens only for the same grant, and advance on larger tokens. A token
does not prove the lease is still live: a resource needing immediate
expiry/revoke enforcement must also validate the current grant and deadline.
A paused old holder is rejected only once the resource has recorded the
successor's higher token; completed or in-flight external writes are not revoked.

## 6. Gap 3 graph TTL wire and feed

### Graph requests and observations

Extend existing `POST /v1/graphs/{graph}/batch` (`graph_write`) with exactly
these two alternatives in its `ops` array. The graph comes from the path.
`expires_at_ms` is **required**, null clears it; `expected` defaults to any.
Node key is 1..256 UTF-8 bytes; edge is exactly `{"key":string}` with the same
bound or `{"id":D}`. Reuse existing graph expected representation [S2].

```json
{"ops":[
  {"op":"set_node_expiry","key":"a","expires_at_ms":"1900000000000","expected":{"revision":"42"}},
  {"op":"set_edge_expiry","edge":{"key":"ab"},"expires_at_ms":null,"expected":"any"}
]}
```

Success retains the existing HTTP 200 batch envelope and outcome kinds:
`{"graph":"g","revision":"43","outcomes":[{"node":{"id":"1","created":false,"revision":"43"}},{"edge":{"id":"2","created":false,"revision":"43"}}]}`.
An empty batch preserves its existing null revision behavior. Set/clear has
no dedicated REST route; create and set may share the same atomic batch.
The 4096 submitted-operation and 16777216-byte engine body limits remain;
the smaller 8 MiB transport cap still applies [S2, T2,
`src/property_graph/engine/limits.rs:15-36`].

Existing POST `/nodes/get` with `{"node":{"key":"a"}}` and `/edges/get` with
`{"edge":{"id":"2"}}` need `graph_read`. Extend **every** serialized node/edge,
including nested traversal/path/retrieval and CLI/binding output, with
`expires_at_ms:Deadline`, the entity's own deadline, not the minimum endpoint
deadline (required on new servers). Envelopes do not change: node fields stay
`id,key,labels,properties,revision` and edge fields
`id,key,relationship,source,source_key,target,target_key,properties,revision`,
each plus `expires_at_ms` [S2]; the read envelope is
`graph,view_sequence,complete,truncation,stats,cursor,node|edge|nodes|edges|paths|shortest_path`
(`src/server/graph.rs:1645-1708`). Missing/due get returns `node:null` or
`edge:null`, **not** a new 404. Old servers omit the field: bindings expose it
as optional and must not read omission as proof of no TTL.

Pin server wall-clock Unix milliseconds once per served read or write;
callers cannot supply a TTL read clock. Due means `deadline <= now`, including
deadline `"0"`. Expiring a node hides every incident edge regardless of the
edge's own TTL; expiring an edge hides only that edge. Reads never mutate.
Writes reclaim first and after an operation that makes entities due, within
the same atomic frame. Later operations cannot clear/resurrect a due entity:
missing node/edge -> 409 `graph_node_not_found` / `graph_edge_not_found`;
revision expectation against a due entity -> 412 `precondition_failed`.
Failed batches reclaim nothing. Wall-clock rollback can change visibility
of not-yet-reclaimed graph entities; this is not historical time travel [T1,
T2]. A proposed lease clock clamp must not silently change graph semantics.

**Attachment invariant:** H-4, REQ-014 and LEG-004 still prohibit expiry of
attached vector records [A1]. Graph-node expiry detaches during reclaim but
does **not** expire or delete the attached record. Setting record expiry while
attached, through vector REST/gRPC/MCP or any future KV path reaching the
record, returns the existing 409 `graph_attached_record_expiry` [A2] (gRPC
FAILED_PRECONDITION with the `vlite-error-code` trailer,
`src/server/grpc.rs:190`; MCP `isError:true`; §10). No bypass by
record ID, batch, import, admin or tenant wildcard is allowed.

### Additive TTL-aware change feed

Propose the `graph_events` feature set from
[wire conventions §4](serving-wire-conventions.md#4-graph-events-in-the-change-feed-c11)
(decision 8): REST `/v1/changes?graph_events=ttl` (or `ttl,temporal` once
temporal lands); gRPC `ChangesRequest.graph_events=3`,
`WatchChangesRequest.graph_events=4` and response echo
`ChangesResponse.graph_events=5` (§7); and optional `graph_events` on admin-only
POST `/v1/admin/webhooks` (`src/server/webhooks.rs:150-164`). The webhook set is
immutable per subscription, existing subscriptions default to the empty set,
and no update route is added. An old server answers 400
`invalid_query` to the unknown query parameter (`ChangeQuery` denies unknown
fields, `src/server/features.rs:90-103`), so clients read the capability flag
first (ledger X4). An older gRPC server silently drops an unknown request
field, so clients also verify the echoed set. Polling, streaming and webhooks
share the decoder, filter and cursor rules in that proposal. The dispatcher
persists progress per feature set; the legacy `webhooks.cursor` path and format
(`src/server/webhooks.rs:42,81-87,405-450`) stay for the empty set.
`FeedCursor` in `src/changes.rs` owns both `vlc1` for the empty set and `vlc2`,
the single non-default feed token, for any non-empty set. The TTL slice lands
`vlc2` once; temporal consumes it. It binds lineage, sequence, ordinal and the
set; a cursor of another set is 409 `cursor_mode_mismatch` (decision (g)).
The former boolean feed proposal is **Withdrawn**: one shared set replaces it
under wire conventions decision 8. Cursor strings stay opaque, and there is
no silent restart from the beginning. Feed authorization is unchanged [AUTH].
The stream already mixes numeric vector-put `expires_at_ms` and
`committed_at_ms` with these decimal-string graph deadlines
(`src/server/features.rs:64-87`); clients must not unify the types.

No new top-level event type or graph `op` enum value is added. TTL-aware
graph batches keep `type:"graph_batch",graph,revision,operations` [F1]; example
`operations` entries:

```json
[
 {"op":"node_patched","id":"1","key":"a","add_labels":[],"remove_labels":[],
  "set":{},"unset":[],"expiry_changed":true,"expires_at_ms":"1900000000000"},
 {"op":"edge_patched","id":"2","key":"ab","set":{},"unset":[],
  "expiry_changed":true,"expires_at_ms":null},
 {"op":"edge_deleted","id":"2","key":"ab","removal_reason":"node_expired",
  "expires_at_ms":"1900000000000","removed_at_ms":"1900000000001"},
 {"op":"node_deleted","id":"1","key":"a","removal_reason":"expired",
  "expires_at_ms":"1900000000000","removed_at_ms":"1900000000001"}
]
```

Field applicability when the set includes `ttl`: node/edge upsert and ordinary patch
carry `expires_at_ms:Deadline` (the resulting own deadline) and
`expiry_changed:false`. A deadline operation emits the corresponding
otherwise-empty patch with `expiry_changed:true`; null clears. Upsert of a new
entity has null until a following deadline patch; upsert/patch of an existing
entity retains its TTL. Explicit deletion adds `removal_reason:"explicit"` and
omits the expiry and removal-clock fields. Expiry deletion adds
`removal_reason:"expired"|"node_expired"`, `expires_at_ms:D` (the effective
deadline causing removal) and `removed_at_ms:D` (the logged reclaim clock, not
the consumer's wall time). An edge's effective deadline is the minimum of its
own and both endpoint deadlines; own-deadline ties use `"expired"`, otherwise
`"node_expired"`. No event is synthesized merely because a reader's clock
passed a deadline; consumers hide cached entities from deadline patches
themselves.

The tracker must gain deadlines, endpoint IDs and attachments and replay every
expiry/reclaim entry [F2] (future Rust work, slice `gap3-rust-feed`, §12). At
each committed reclaim, emit due edge deletions by ascending ID, detach events
by `(node ID,collection,record ID)`, then due node deletions by ascending ID,
each exactly once; detach events keep their schema. Flush at vector/lifecycle
boundaries as today (`src/property_graph/feed.rs:1-10,90-100`) and preserve
frame sequence and logged order, including set-then-immediate-reclaim. The empty
set keeps whole commits on a page (`src/changes.rs:445-448`,
`src/openapi.json:33`). Per the proposed wire conventions decision 9, only TTL
reclaim runs may split a commit; temporal commits stay whole. Split a TTL
reclaim run greedily at 256 operations or 1 MiB of canonical JSON event
bytes, whichever comes first, without reordering; one larger operation is
allowed up to the 16 MiB response cap. Each chunk is an event numbered by its
`ordinal` (`src/changes.rs:163-170`) and the `vlc2` cursor carries the last
delivered ordinal; consumers must not assume one event per commit or treat a
page as a transaction. Never truncate a reclaim expansion or emit an
unresumable empty page.

Maintenance must keep a feed resume path to every committed reclaim before
dropping its history, or invalidate the lineage and cursors explicitly.
Compaction or checkpoint removal without a logged reclaim creates no fictitious
delete event; bootstrap/export include deadlines, and a restart must not infer
removals from the reader's clock. Vector-put `expires_at_ms=None` at [F2] is
unchanged: graph TTL is not record TTL. Old consumers stay on the empty set;
opted-in consumers must tolerate additive fields (tested, not assumed).

## 7. Proposed proto additions

Proposal for package `vlite.v1`, pending owner ratification
(§12 ledger); it edits no `.proto` file. It is additive only
(`serving-contract.md:209`): existing RPCs, messages and tags stay unchanged
and no number is reused. Field allocation and domain-message reserves follow
[wire conventions §1.3](serving-wire-conventions.md#13-field-number-allocation-c1)
and §1.4 (decision 1, proposed). **Withdrawn:** the former no-reserves
proposal conflicts with those shared-message bands. `Expected` is reused with
fields 1/2/3 [P1]; an unset `expected` or empty `kind` means any
(`proto/vlite/v1/vlite.proto:112`, `src/server/grpc.rs:378-385`). Fields required
by the contract are validated (empty `collection` is `invalid_input`, §4). An
absent optional expiry means no TTL on a KV put and a clear on a graph expiry
write. Bytes are raw proto bytes, not base64.

```proto
service Kv {
  rpc GetKvEntry(KvGetRequest) returns (KvEntry);
  rpc PutKvEntry(KvPutRequest) returns (KvPutResponse);
  rpc DeleteKvEntry(KvDeleteRequest) returns (KvDeleteResponse);
  rpc ListKvEntries(KvListRequest) returns (KvListResponse);
  rpc AcquireLease(KvAcquireLeaseRequest) returns (KvLease);
  rpc RenewLease(KvRenewLeaseRequest) returns (KvLease);
  rpc RevokeLease(KvRevokeLeaseRequest) returns (KvRevokeLeaseResponse);
}
message KvAddress {
  reserved 100 to 199;
  string collection = 1; string namespace = 2; string key = 3;
}
message KvGetRequest { reserved 100 to 199; KvAddress address = 1; }
message KvEntry {
  reserved 100 to 199;
  string key = 1; bytes value = 2; uint64 version = 3;
  optional uint64 expires_at_ms = 4;
}
message KvPutRequest {
  reserved 100 to 199;
  KvAddress address = 1; bytes value = 2; Expected expected = 3;
  optional uint64 expires_at_ms = 4;
}
message KvPutResponse {
  reserved 100 to 199;
  uint64 version = 1; optional uint64 expires_at_ms = 2;
}
message KvDeleteRequest {
  reserved 100 to 199;
  KvAddress address = 1; Expected expected = 2;
}
message KvDeleteResponse { reserved 100 to 199; bool deleted = 1; }
message KvListRequest {
  reserved 100 to 199;
  string collection = 1; string namespace = 2; string prefix = 3;
  optional string after = 4; optional uint32 limit = 5;
}
message KvListResponse {
  reserved 100 to 199;
  repeated KvEntry items = 1; optional string next_after = 2;
}
message KvAcquireLeaseRequest {
  reserved 100 to 199;
  KvAddress address = 1; uint32 ttl_ms = 2;
}
message KvRenewLeaseRequest {
  reserved 100 to 199;
  KvAddress address = 1; uint64 fencing_token = 2; uint32 ttl_ms = 3;
  string fence_domain = 4;
}
message KvRevokeLeaseRequest {
  reserved 100 to 199;
  KvAddress address = 1; uint64 fencing_token = 2; string fence_domain = 3;
}
message KvLease {
  reserved 100 to 199;
  string namespace = 1; string key = 2; string fence_domain = 3;
  uint64 fencing_token = 4; uint64 expires_at_ms = 5;
}
message KvRevokeLeaseResponse { reserved 100 to 199; bool revoked = 1; }

// Append these RPCs to the Gap 12 Graph service (decision 2, proposed).
service Graph {
  rpc GetNodeExpiry(GraphExpiryNodeRequest) returns (GraphExpiryResponse);
  rpc GetEdgeExpiry(GraphExpiryEdgeRequest) returns (GraphExpiryResponse);
  rpc WriteExpiryBatch(GraphExpiryBatchRequest) returns (GraphExpiryBatchResponse);
}
message GraphExpiryNodeRequest {
  reserved 100 to 199;
  string graph = 1; string key = 2;
}
message GraphExpiryEdgeRequest {
  reserved 100 to 199;
  string graph = 1; GraphSelector edge = 2;
}
message GraphExpiry {
  reserved 100 to 199;
  uint64 id = 1; optional string key = 2; uint64 revision = 3;
  optional uint64 expires_at_ms = 4;
}
message GraphExpiryResponse {
  reserved 100 to 199;
  string graph = 1; uint64 view_sequence = 2; optional GraphExpiry entity = 3;
}
message GraphExpirySetNode {
  reserved 100 to 199;
  string key = 1; optional uint64 expires_at_ms = 2; Expected expected = 3;
}
message GraphExpirySetEdge {
  reserved 100 to 199;
  GraphSelector edge = 1; optional uint64 expires_at_ms = 2;
  Expected expected = 3;
}
message GraphExpiryOperation {
  reserved 100 to 199;
  oneof operation { GraphExpirySetNode node = 1; GraphExpirySetEdge edge = 2; }
}
message GraphExpiryBatchRequest {
  reserved 100 to 199;
  string graph = 1; repeated GraphExpiryOperation ops = 2;
}
message GraphExpiryWrite {
  reserved 100 to 199;
  uint64 id = 1; uint64 revision = 2; bool node = 3;
}
message GraphExpiryBatchResponse {
  reserved 100 to 199;
  string graph = 1; optional uint64 revision = 2;
  repeated GraphExpiryWrite outcomes = 3;
}
```

Gets project the REST get result (missing or due -> absent `entity`); node-get
takes a key, edge-get a key or ID through the shared `GraphSelector`.
**Withdrawn:** the separate service (formerly `GraphTtl`) duplicates graph
registration; wire conventions decision 2 proposes folding its RPCs into `Graph`.
**Withdrawn:** the duplicate edge selector (formerly `GraphExpiryEdge`) is
replaced by `GraphSelector` under wire conventions §1.3.

`WriteExpiryBatch` stays only as a TTL-only wrapper of `GraphExpiryOperation`,
with ordered outcomes (`created:false` implied, `node=false` meaning edge).
Gets need `graph_read`, writes `graph_write`. A mixed create-and-TTL batch
uses one `WriteGraphBatch` call (`serving-contract.md:152`), with this proposed
addition inside the Gap 12 `GraphWriteOperation.operation` oneof:

```proto
GraphExpiryOperation expiry = 400;
```

The TTL slice carves tag 400 from the shared message's band fence, leaving
`reserved 401 to 499`, per wire conventions §1.3; it does not add direct TTL
fields to that shared message.

The KV slice registers `vlite.v1.Kv` on the shared routes and adds its name to
`GrpcHealth::set`, the `set_not_serving` doc comment and the `grpc_smoke` health
loop, applying both caps to `KvServer` per
[wire conventions §1.2](serving-wire-conventions.md#12-per-service-obligations-c14).
TTL uses the Gap 12 `Graph` registration, health and caps, not another service.
The descriptor test checks service/RPC short-name uniqueness and named
additions, including the tag-400 arm, per §§1.2-1.4 and decision 16 (proposed).

Additive edits to existing feed messages (these lines are inserts, not
replacement declarations; every existing tag stays untouched):

```proto
// ChangesRequest has fields 1-2 today (vlite.proto:455):
repeated GraphEventFeature graph_events = 3;
// WatchChangesRequest has fields 1-3 today (vlite.proto:456):
repeated GraphEventFeature graph_events = 4;
// ChangesResponse has fields 1-4 today (vlite.proto:525):
repeated GraphEventFeature graph_events = 5;
// New enum GraphEventFeature has values TTL and TEMPORAL (wire conventions §1.3).
// New enum:
enum GraphRemovalReason {
  GRAPH_REMOVAL_REASON_UNSPECIFIED = 0;
  GRAPH_REMOVAL_REASON_EXPLICIT = 1;
  GRAPH_REMOVAL_REASON_EXPIRED = 2;
  GRAPH_REMOVAL_REASON_NODE_EXPIRED = 3;
}
// GraphChangeOperation has fields 1-18 today (vlite.proto:487-507):
optional uint64 expires_at_ms = 19;
bool expiry_changed = 20;
GraphRemovalReason removal_reason = 21;
optional uint64 removed_at_ms = 22;
```

In TTL-aware upsert/patch an absent `expires_at_ms` means JSON null and
`expiry_changed` separates a clear from an ordinary patch; without `ttl` the
four new fields stay unset. `ChangeEvent.expires_at_ms` (field 7) keeps its
vector-record meaning. A server that predates a new service answers
UNIMPLEMENTED; clients report it and never emulate leases. The tags 3/4/5 and
19-22, the enums, the `Graph` additions and the `Kv` service remain proposals:
the amendment of `serving-contract.md:206` and the Gap 8/12 tag check are
ledger X1 (§12). The TTL slice registers `GraphChangeOperation.expires_at_ms=19`,
`expiry_changed=20`, `removal_reason=21`, `removed_at_ms=22`, all
`GraphRemovalReason` names/values, and the feature enum and request/echo fields
as named descriptor additions under wire conventions §§1.3 and 5.3, rather
than silently weakening the baseline descriptor-equivalence assertion.

## 8. Proposed OpenAPI and reference delta

A proposal for the OpenAPI 3.1 description (`src/openapi.json:2`); it authorizes
no edit of the frozen `src/openapi.json` or `src/openapi_graph.json`
(`next-major.md:239-241`). Neither file has a `tags` array, so none is added.
Paths, methods, operation IDs and scopes are the §4 table; request and response
objects are the §3 types, with `additionalProperties:false` on inputs. Existing
collection routes name the path parameter `c` (`src/openapi.json:54-57`);
`{collection}` is notation only. `KvDelete` takes query parameters `namespace`,
`key` and `expected` (string matching `^(any|absent|(0|[1-9][0-9]{0,19}))$`,
default `any`) and no `requestBody`. Every operation inherits BearerAuth and the
unchanged `Error` envelope, and documents 429 `rate_limited` / `quota_exceeded`
with `Retry-After` (`src/server.rs:1408-1421`). Ordinary KV write responses say
"acknowledged after commit; synced only under `--durability full`"; lease write
responses say "synced" (decision (a)). The global sentence "Successful writes are
synced." (`src/openapi.json:6`) needs the same qualifier (ledger X2).

New component schemas must live in `src/openapi.json`: `merge_openapi` copies
only `paths` from `openapi_graph.json` into the served document
(`src/server/graph.rs:1757-1786`). `KvDecimal` is §3's `D`; `KvPut` and
`KvEntry` use `KvValue` for `value_base64`, lease messages use `FenceDomain`,
and every other schema is its §3 table row:

```json
{"components":{"schemas":{
 "KvDecimal":{"type":"string","pattern":"^(0|[1-9][0-9]{0,19})$","x-maximumUint64":"18446744073709551615"},
 "KvNamespace":{"type":"string","minLength":1,"x-maxUtf8Bytes":255},
 "KvKey":{"type":"string","minLength":1,"x-maxUtf8Bytes":1024},
 "KvValue":{"type":"string","contentEncoding":"base64","maxLength":1396388,"x-maxDecodedBytes":1047290},
 "FenceDomain":{"type":"string","pattern":"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"},
 "KvExpected":{"oneOf":[{"enum":["any","absent"]},{"type":"object","additionalProperties":false,"required":["revision"],"properties":{"revision":{"$ref":"#/components/schemas/KvDecimal"}}}]},
 "GraphExpiryExpected":{"oneOf":[{"enum":["any","absent"]},{"type":"object","additionalProperties":false,"required":["revision"],"properties":{"revision":{"oneOf":[{"$ref":"#/components/schemas/KvDecimal"},{"type":"integer","minimum":0}]}}}]},
 "GraphExpiryOperation":{"oneOf":[{"$ref":"#/components/schemas/GraphExpirySetNode"},{"$ref":"#/components/schemas/GraphExpirySetEdge"}]},
 "GraphExpirySetNode":{"type":"object","additionalProperties":false,"required":["op","key","expires_at_ms"],
  "properties":{"op":{"const":"set_node_expiry"},"key":{"type":"string","minLength":1,"x-maxUtf8Bytes":256},
   "expires_at_ms":{"anyOf":[{"$ref":"#/components/schemas/KvDecimal"},{"type":"null"}]},
   "expected":{"$ref":"#/components/schemas/GraphExpiryExpected","default":"any"}}},
 "GraphExpirySetEdge":{"type":"object","additionalProperties":false,"required":["op","edge","expires_at_ms"],
  "properties":{"op":{"const":"set_edge_expiry"},
   "edge":{"oneOf":[{"type":"object","additionalProperties":false,"required":["key"],"properties":{"key":{"type":"string","minLength":1,"x-maxUtf8Bytes":256}}},
    {"type":"object","additionalProperties":false,"required":["id"],"properties":{"id":{"oneOf":[{"$ref":"#/components/schemas/KvDecimal"},{"type":"integer","minimum":0}]}}}]},
   "expires_at_ms":{"anyOf":[{"$ref":"#/components/schemas/KvDecimal"},{"type":"null"}]},
   "expected":{"$ref":"#/components/schemas/GraphExpiryExpected","default":"any"}}}
}}}
```

`x-*` bounds need explicit runtime validation: JSON Schema cannot count UTF-8
bytes or bound a uint64 string, and a violation is 422 `invalid_input` (§3).
Nonzero is enforced only for issued versions and fencing tokens, not for graph
IDs, expected revisions or deadlines. `GraphExpiryExpected` keeps the existing
integer revision input beside `D`: the parser accepts a canonical decimal string
or a non-negative integer (`src/bin/tool_support/graph_json.rs:98,161-168`).

The graph fragment is inline, not named components; the batch path is
`src/openapi_graph.json:2115`, `GraphBatch` `:2128`, its op list `:2146` and op
enum `:2149-2161`; `GraphGetNode` (`:394`) and `GraphGetEdge` (`:635`) keep their
IDs. Batch `items` are `additionalProperties:true` today, hence the conditional
refs. After the components are added, the batch item schema gains
`set_node_expiry` and `set_edge_expiry` in its `op` enum and one TTL-domain
`allOf` `if`/`then` branch: `if` matches either `op` const via `anyOf`, and
`then` `$ref`s `GraphExpiryOperation`. The `:2146` description gains the same
two op names. This follows
[wire conventions §5.2](serving-wire-conventions.md#52-per-file-owner-and-sequence),
decision 23 (proposed); `items` stays one open object, not a `oneOf`, and
`ops.maxItems` stays 4096. **Withdrawn:** the former two-branch TTL proposal
does not match that one-branch-per-domain mechanism.

Node and edge response projections gain a required `expires_at_ms`
(`KvDecimal` or null) inside each entity object, including objects the inline
responses admit only through `additionalProperties:true`; the graph-read
envelope does not change (§6). `GET /v1/changes` gains optional query
`graph_events` feature set and webhook registration the immutable set from
§6; the response echoes `graph_events`. TTL-aware feed schemas add the §6
fields and the empty-set schemas stay as an alternative. No other path, operation ID or
schema change is proposed.

Reference rows: every new public API needs a `site/api.html` row and a
`data-api` key before implementation merges, because `tests/api_reference.py:4-7`
fails on both missing and stale keys; this document adds no row (ledger X6). The
extractor's families are listed at `tests/api_reference.py:9-23`. Proposed keys:
`rest:METHOD /v1/...`, `mcp:vlite_kv_get` etc., unqualified `grpc:<Rpc>`
(short names stay unique under wire conventions decision 16, proposed;
no namespaced extractor change) and `rust:` rows for new Rust methods (existing KV rows:
`site/api.html:1062-1074`). The HTTP clients and the CLI have no extractor
family, so the client slice adds `http-py`, `http-js`, `dotnet` and `cli`
families with the same rule.

## 9. MCP, CLI and client/binding parity

MCP tools are `vlite_kv_get`, `vlite_kv_put`, `vlite_kv_delete`,
`vlite_kv_list`, `vlite_kv_lease_acquire`, `vlite_kv_lease_renew` and
`vlite_kv_lease_revoke`. Each `inputSchema` is the §3 type plus a required
`collection:string` (the vector tools default it, `src/bin/vlite-mcp.rs:458`; KV
tools never assume one), with `additionalProperties:false` and the §3 bounds.
Existing tools declare only an `inputSchema` and `annotations`
(`src/bin/vlite-mcp.rs:464-474`), so no `outputSchema` is added. Results keep the
existing envelope: success is `structuredContent` plus one identical text block;
failure is `isError:true` with `structuredContent.error` a **string**
(`src/bin/vlite-mcp.rs:673-682`), which new tools begin with the stable code
(`<code>: <message>`, as `src/bin/tool_support/graph_mcp.rs:56-58`). Malformed
`params`, or a `tools/call` without a name and object arguments, is JSON-RPC
-32602 (`src/bin/vlite-mcp.rs:702-708,766-772`); an unknown argument key fails
validation (`:294-320`). `vlite_graph_write` carries both TTL ops and the graph
read tools add deadline fields; no TTL-specific tool is added.

A local `vlite-mcp --db` session owns the file, has no credential model, is
read-only by default and lists write tools, including the KV mutation and lease
tools, only with `--writable` (`src/bin/vlite-mcp.rs:23-27,722-726`; a read-only
session answers `read_only: ...`, `src/bin/tool_support/graph_mcp.rs:111-115`);
its lease holder is that single local owner. Proxy mode (`--server URL`) opens
no file and forwards only graph tools to REST so the server authorizes them
(`src/bin/tool_support/graph_proxy.rs:1-9`), so a KV tool is "unknown" there
today (`src/bin/vlite-mcp.rs:762-764`); forwarding to the §4 routes, so that §10
applies, is ledger X2.

The existing `vlite-shell` gains `kv-get`, `kv-put`, `kv-delete`, `kv-list`,
`kv-lease-acquire`, `kv-lease-renew` and `kv-lease-revoke`, with no new binary,
flag or grammar: one `COMMAND REQUEST` per line
(`src/bin/vlite-shell.rs:11-21,54-101`), REQUEST being the MCP input JSON
including `collection`, so a line never depends on `use NAME`. `--read-only`
refuses KV and lease mutations as it does `put` (`src/bin/vlite-shell.rs:101`).
Each command prints one JSON object. Exit status follows the existing tools
(`src/bin/tool_support/mod.rs:5-13`): 0, or 1 with `error: <code>: <message>` on
stderr; there is no exit code 2. A script stops at its first error and a
terminal session prints it (`src/bin/vlite-shell.rs:226-232`). The CLI never
retries or creates a collection. Lease commands use the single local-owner
holder and, per §5, a lease does not survive the process. `vlite-graph-cli --db
PATH --writable batch REQUEST` (`src/bin/vlite-graph-cli.rs:62`) carries the same
TTL ops and deadline fields [S2].

The HTTP clients gain one method per operation, mapping exactly to §4 and
taking the §3 fields in each client's existing style (Python keyword-only,
JavaScript options object, .NET positional plus `CancellationToken cancellation
= default`; compare `changes` at `clients/python/vlite_client/__init__.py:911`,
`clients/javascript/index.js:376`, `clients/dotnet/VLite.Client/Client.cs:101`).
For the operations `get`, `put`, `delete`, `list`, `lease_acquire`,
`lease_renew` and `lease_revoke` the names are Python `kv_<op>`, JavaScript
`kv<Op>` and .NET `Kv<Op>Async`, such as `kv_lease_acquire`, `kvLeaseAcquire`
and `KvLeaseAcquireAsync`.

The value is `bytes`, `Uint8Array` or `byte[]` under `value` (not
`value_base64`); only the adapter encodes base64. D fields are Python `int`,
JavaScript `bigint` and .NET `ulong`, serialized as D strings and never through
a JavaScript Number (the client already validates 64-bit strings,
`clients/javascript/index.js:1-5,107-114`). `Expected` has Any, Absent and
Revision variants; .NET records use PascalCase with the §3 snake_case JSON names
(`clients/dotnet/VLite.Client/Client.cs:35-39`). Graph TTL helpers are
`graph_set_node_expiry` / `graphSetNodeExpiry` / `GraphSetNodeExpiryAsync` plus
the edge methods, each submitting one batch op and returning the write outcome
(`id,created:false,revision`). The existing feed methods return untyped JSON
(`clients/javascript/index.d.ts:273`) and gain an optional `graph_events` /
`graphEvents` set and verify its echo; deadlines arrive as D strings, and one absent from an older server's
response stays unknown, never null. A server without the feature gives an
explicit unsupported error, never a client-side fallback, and no client retries
a mutation after cancellation.

**Bindings (decision (i)).** No binding exposes KV today. Python and Node
embedded wrap the C library
(`bindings/python/vlite_embedded/__init__.py:44,489,548`,
`bindings/node/index.d.ts:181,195,274-290`); WASM is independent
(`bindings/wasm/src/lib.rs:149-165,251-262`); the graph batch parser exists
twice, as `OpIn` in `src/bin/tool_support/graph_json.rs:208` (CLI, MCP, REST)
and in the C ABI (`bindings/c/src/graph.rs:191`). Recommended scope: the C ABI
carries only the graph TTL ops, deadline result fields and the `graph_events` feed
option (`bindings/c/src/features.rs:820-828`), so `graph_write_batch` /
`graphWriteBatch` accept them; embedded KV and lease methods wait for the §12
Rust slices; WASM keeps the default feed and reports an explicit unsupported
error for TTL. HTTP clients and the CLI get full parity; .NET is HTTP only.

## 10. Authorization, failure mapping and observability

**Scopes (decision (d)).** No new credential scope: a scope name an older
build does not know makes its credential store fail to load
(`src/server/credentials.rs:41-42`). KV get and list need `read`; KV put,
delete and every lease operation need `write`; each also needs a binding for
the addressed collection (`src/server/credentials.rs:25-55,160-170`). A
tenant-local `collections:["*"]` therefore covers every KV-bearing collection
of its tenant: dedicated collections (§3) are the isolation boundary, the
namespace is not. Graph TTL writes need `graph_write` and reads `graph_read`,
each with a graph binding. Collection scope never implies graph scope (403
`forbidden_scope`, even with `collections:["*"]`) and graph scopes never
authorize KV (`src/server/graph.rs:15-35`, GATE-007). A TTL operation whose
reclaim detaches records also needs `write` and a binding for every stored
collection detached, checked as stored (never tenant-qualified) against the
full due/cascade set under the write guard before any change, for
batch-triggered reclaim as well. Deferred maintenance is a system operation,
not a tenant bypass. Feed access keeps its read admission plus `graph_read` and
the graph binding per graph event; hidden events still advance the cursor
(`src/server/graph.rs:85-95`) [AUTH]. Dedicated `kv_read`/`kv_write` scopes
remain a future option.

**Tenancy.** Collection and graph names are qualified once with the
authenticated tenant, never the namespace or key: a tenant principal
addressing `g` reaches only `t{tenant}__g`
(`src/server/credentials.rs:141-150`). The lease holder is the credential
identity (§5), bound to the physical collection ID and database lineage, so a
recreated collection never matches (`src/server/credentials.rs:160-170`).
`collections:["*"]` and `graphs:["*"]` stay tenant-local. Authorization runs
before lookup, so a denial never reveals whether a resource exists. Admin and
read-only tokens see stored names; the read-only token may get, list and read,
never write, acquire, renew or revoke (`read_only`, `src/server.rs:988-995`).
No role bypasses an invariant.

**Rate and quota.** A scoped credential with `requests_per_second` spends one
bucket token per REST request and gRPC call, failed lease attempts included; an
empty bucket is 429 `rate_limited` with `Retry-After: 1`, a full limiter table
503 `busy`, and admin and read-only roles are not limited today (`rate_limit`,
`src/server.rs:3474-3506`, called at `src/server.rs:1134` and
`src/server/grpc.rs:264`; header `src/server.rs:1414-1419`). Proxy-mode MCP
calls are REST requests and spend tokens the same way; local MCP has no
credential. KV puts count against live-record quotas because entries are
ordinary records (`enforce_quota`, `src/server.rs:3518`; error at `:3577-3580`).
Lease state needs a finite per-tenant admission quota configured before
enablement (decision (k)); exhaustion is `quota_exceeded`, never eviction of a
live grant or the high-water. No new sweeper or queue is authorized.

**Errors.** REST failures use `{"error":{"code","message"}}`
(`src/server.rs:1407-1421`); messages are not a parsing contract. gRPC adds the
`vlite-error-code` trailer and `x-request-id` (`src/server/grpc.rs:199-216`);
MCP puts the code first in a string (§9). A 412 stays ABORTED
(`src/server/grpc.rs:189`) [WIRE].

| Error | REST | gRPC |
|---|---|---|
| `unauthorized` | 401 | UNAUTHENTICATED |
| `forbidden_scope`, `read_only`, binding or tenant denial | 403 | PERMISSION_DENIED |
| `collection_not_found`, `kv_not_found` | 404 | NOT_FOUND |
| `invalid_query` (query-string syntax, duplicate parameter) | 400 | INVALID_ARGUMENT |
| `invalid_json` (extractor: syntax 400, data 422, body over 8 MiB 413; decision (h)) | 400, 413, 422 | none; tonic's own `OUT_OF_RANGE` covers gRPC size violations (§3) |
| `invalid_input` (value breaks a bound or canonical form; engine `InvalidKvKey`, `PayloadTooLarge`; empty gRPC `collection`) | 422 | INVALID_ARGUMENT |
| `precondition_failed` (`RevisionMismatch`; absent current included) | 412 | ABORTED |
| `not_kv_collection`, `kv_slots_exhausted`, `lease_held`, `stale_fencing_token`, `lease_unavailable`, `fencing_recovery_required`, `cursor_mode_mismatch`, `upgrade_required`, `graph_node_not_found`, `graph_edge_not_found`, `graph_attached_record_expiry` | 409 | FAILED_PRECONDITION |
| `rate_limited`, `quota_exceeded` | 429 | RESOURCE_EXHAUSTED |
| `busy`, `unavailable` | 503 | UNAVAILABLE |
| `commit_indeterminate` (may already be committed) | 503 | UNKNOWN |
| `internal_error` (logged; never a validation fallback) | 500 | INTERNAL |

The engine's `InvalidKvKey`, `KvSlotsExhausted` and `NotKvCollection`
(`src/lib.rs:475-479`) have no server arm and would reach
`error => internal(error)`, HTTP 500 (`src/server.rs:1624`), and the tool
layer's `database_error` fallback (`src/bin/tool_support/graph_json.rs:26-72`);
the mapping rows are ledger X3 (§12), and nothing here is claimed served. Other
graph errors keep their names and statuses [A2]. A stale token and a foreign
holder deliberately share `stale_fencing_token`.

New-code producers follow
[wire conventions §2](serving-wire-conventions.md#2-error-registry-c2-c9):
`kv_not_found` is an `ApiError` built by the KV get handler for an absent/due
entry; `kv_slots_exhausted` and `not_kv_collection` are new `From<Error>` arms
for existing `KvSlotsExhausted` and `NotKvCollection`. The existing
`InvalidKvKey` gains an arm for `invalid_input`, not a new code. Lease handlers
build `ApiError`s for `lease_held`, `stale_fencing_token`, `lease_unavailable`
and `fencing_recovery_required`; the feed adapter builds the `ApiError` for
`cursor_mode_mismatch` after checking the cursor's feature set. None adds a
public `Error` variant.

**Audit and metrics.** Mutations use the existing audit log
(`src/server/audit.rs:1-5`: entries never hold secrets, vectors, payloads or
metadata values). Fields: mutation name, request ID, tenant-safe principal ID,
authorized collection or graph, lease domain, outcome code and duration; never
values, raw keys, namespaces, bearer tokens or whole requests. Audit stays
non-blocking, as today: a write failure increments `audit_failures` and never
undoes the change (`src/server/collections.rs:172-183`). The existing counters
(`credential_denials_total`, `rate_limited_total`, `quota_exceeded_total`,
`audit_write_failures_total`; `src/server/observability.rs:136-139`) gain
siblings for lease acquire/renew/revoke/conflict/stale, TTL set/clear, feed
deadline and reclaim events, errors and admission rejection, labeled only by
bounded operation, transport and outcome. Cross-transport error and
authorization goldens are required before any route or tool is enabled.

## 11. Independent acceptance matrix

The independent Python oracle models the requirements directly, not Rust's
hash, probing, revision allocator or codec: a dictionary keyed by
`(tenant,collection,namespace,key)`, a separate lease table with durable
high-water and domain, an injected wall clock and a downstream highest-token
model. Revisions are opaque increasing values; assertions compare relations and
precondition outcomes, never allocation counts. Deterministic operation
sequences compare visible state, deadline/null presence, ordering, error codes,
grants and tokens; Rust collision fixtures cover the 16-slot probe separately.

| Area | Required fixtures / oracle assertions |
|---|---|
| Exact wire goldens | Every route/RPC/tool success and error; empty bytes, non-ASCII keys, all Expected alternatives, null vs omitted expiry, u64 maximum and JS-safe-integer boundary |
| Bounds | Namespace 0/1/255/256 bytes; key 0/1/1024/1025; multibyte lengths; values 1047290/1047291 decoded bytes; list 0/1/10000/10001; prefix/after bounds; canonical and malformed base64 |
| KV concurrency | Two absent writers, two CAS writers, delete/recreate ABA, hash collision/deletion hole, expired occupied slots; exactly one eligible conditional write wins |
| Pagination | UTF-8 lexical ties/prefixes, exclusive after, writes between pages, due keys, exact limit and exact encoded-response boundary; no empty continuing page |
| TTL | Injected deadline-1/deadline/deadline+1; zero/max; clear/live revision bump; upsert preserves; KV put clears; one fixed batch clock; no clock override over wire |
| Leases | Acquire/renew/revoke/expire; equal/newer/older token; cross-holder, cross-namespace/tenant/collection; overflow and max TTL; downstream rejects predecessor after successor |
| Durability and crash | Ordinary KV is acknowledged after the handle's commit and synced only under `full`; under `periodic`/`none` assert only the documented loss window; lease writes always sync or the route refuses; faults before allocation sync, after high-water sync, after grant sync before reply, during revoke/renew and during the recovery barrier; never repeat an acknowledged or burned token |
| Maintenance and recovery | Restart revokes live grants; compaction, upgrade, re-encryption, `convert_to_directory` and checkpoint carry the ledger forward; drop/recreate never resets it; an older reader refuses the file; backup, PITR and checkpoint restore, copy, rollback and promotion give `fencing_recovery_required` |
| Graph invariant | Node cascade and independent edge expiry; shared edges counted once; >4096 affected edges under one reclaim; rollback removes nothing; attached record survives with no expiry |
| Feed | Empty-set byte goldens and legacy webhook progress unchanged; per-set webhook progress; single `changes.rs`-owned `vlc2` resume inside TTL reclaim and `cursor_mode_mismatch` across sets; request/response set echo and old-server detection; set/clear/removal/detach goldens; vector puts stay null; deterministic reclaim chunks and ordinals, whole temporal commits; no wall-clock synthetic delete |
| Proto descriptors | Unique RPC short names, `vlite.v1.Kv` health and both caps, TTL RPCs on `Graph`; `GraphSelector` reuse; mixed create/expiry through tag 400; named feature fields 3/4/5, `GraphChangeOperation` fields 19-22 and `GraphRemovalReason` values |
| Parity/auth | REST/gRPC/MCP share oracle traces and tenant qualification; scope/binding denials precede existence; read-only denial; namespace is not an authorization boundary; `graph_*` scopes never authorize KV and `collections:["*"]` never grants graph scope; immediate-reclaim collection checks; rate and size limits |
| Bindings/CLI | Python/JS/.NET lossless conversions; embedded parity; MCP code-first string errors; local MCP read-only; proxy KV tool unknown until ledger X2; stdin errors and exit status; old-server missing-field and UNIMPLEMENTED behavior; no retry on an ambiguous mutation |

Mutation tests must kill each of: `<` for `<=`; character count for UTF-8
bytes; hash-only identity; inclusive `after`; missing prefix filter; expired
key matching a revision; ignored `Expected`; reused token after restart;
renewal allocating a token; absent holder check; forgotten high-water or
recovery barrier; restore resetting the domain; deadline clear read as
omission; skipped endpoint cascade; attached record receiving node TTL; missing
detach; reordered reclaim chunks; reader clock in the feed; another-set cursor
accepted for `graph_events=ttl`; wildcard implying `graph_write`; JS Number conversion;
success before sync; automatic retry. Each must change a recorded assertion.

## 12. Delivery slices, ledger and owner decisions

Nothing here is implemented. New features land as embedded Rust APIs first
(`next-major.md:239-241`), so the lease ledger and the feed tracker are Rust
slices that precede their serving slices; no KV batch slice exists (decision
(p)). Each slice is accepted independently against its dependencies, ships its
own schema, goldens and oracle tests (SHIP-001) and owns the ledger rows below
that touch its surface. The lease ledger is never mixed with graph feed work.
The file lists are what a future slice would own; this document edits none of
them.

| Slice (depends on) | Files owned in a future implementation | Required tests and acceptance (the independent reviewer signs) |
|---|---|---|
| `gap2-rust-lease-ledger` (decisions (a)-(c); the P2 converter, `next-major.md:237-238`) | `src/kv.rs`, a new ledger module, `src/lib.rs` hooks, a `src/format.rs` section and capability flag, `src/maintenance.rs`, `src/logops.rs`, `src/directory.rs`, `src/replica.rs` | §11 durability, crash and maintenance rows; an older reader refuses the file; LEASE-001/002 |
| `gap2-kv-serving` (none: get, put, delete and list exist [K2]) | `src/server.rs`, a new server KV adapter, `src/server/grpc.rs`, credential, tenant, audit and quota hooks, `src/bin/vlite-mcp.rs`, `src/bin/tool_support/graph_proxy.rs`, `src/bin/vlite-shell.rs`, the Python, JavaScript and .NET clients | KV oracle, wire goldens, CAS races, bounds, pagination, auth and parity; `vlite.v1.Kv` health, both caps and named descriptor entries with unique RPC short names (§7); KV-001/002, AUTH-001, SHIP-001 |
| `gap2-lease-serving` (`gap2-kv-serving`, `gap2-rust-lease-ledger`) | the lease routes, RPCs, tools, CLI commands and client methods of §4, §7 and §9 | lease oracle, downstream-token model, crash and restore, quota and rate cases; LEASE-001/002, AUTH-001, SHIP-001 |
| `gap3-rust-feed` (none) | `src/changes.rs` (`vlc2`, ordinals), `src/property_graph/feed.rs` (tracker for deadlines, endpoint IDs, attachments, expiry and reclaim replay) | replay oracle, default-feed byte goldens, chunk and cursor boundaries, maintenance and restart; FEED-001. [Rust TTL feed](gap3-ttl-feed.md): design candidate pending review. |
| `gap3-feed-events` (`gap3-rust-feed`) | `src/server/features.rs`, `src/server/grpc.rs`, `src/server/webhooks.rs` (progress per feature set), feed authorization, feed encoders such as `bindings/c/src/features.rs` | server feed goldens, per-set webhook progress, feature-set echo, named descriptor additions for fields 3/4/5, 19-22 and both enums, `cursor_mode_mismatch`, filter parity; FEED-001, AUTH-001, SHIP-001 |
| `gap3-ttl-serving` (`gap3-feed-events` and the Gap 12 `Graph` schema: the feed skips TTL entries today [F2], so TTL writes are not served before it can replay them) | `src/server/graph.rs`, `src/bin/tool_support/graph_json.rs`, `graph_mcp.rs`, `src/bin/vlite-graph-cli.rs`, the C ABI op parser, gRPC `Graph` additions and tag-400 arm, the clients and bindings | clock-boundary oracle, all projections, mixed create and TTL atomicity through `WriteGraphBatch`, wrapper parity, named tag-400 descriptor entry, missing and due preconditions, attachment invariant, >4096 cascade, auth, parity and client goldens; TTL-001/002, ATTACH-001, AUTH-001, SHIP-001 |

**Delivery ledger: obligations on other files.** Each serving slice carries its
share of these rows and states in prose only what it ships.

| ID | Files | Obligation |
|---|---|---|
| X1 | `docs/projects/roadmap/serving-contract.md` | Amend `serving-contract.md:206` ("keep GraphChangeOperation and existing stream messages unchanged") to permit the named additive `GraphChangeOperation` fields 19-22, `graph_events` request tags 3/4 and echo 5, `GraphEventFeature` and `GraphRemovalReason` (§7); append TTL RPCs to `Graph` and carve the `GraphExpiryOperation expiry = 400` arm under wire conventions §§1.1-1.4 (decisions 1 and 2, proposed); compare tags and reserved ranges with the Gap 8/12 schemas and register named descriptor additions |
| X2 | `src/openapi.json`, `src/openapi_graph.json`, `proto/vlite/v1/vlite.proto`, MCP schemas, graph parsers | `src/openapi.json`: KV paths, the §8 components, the `graph_events` query, echo and webhook property, error rows and a qualifier on the sentence at `src/openapi.json:6` "Successful writes are synced."; `src/openapi_graph.json`: the §8 batch-item changes, projections and the op description at `src/openapi_graph.json:2146`; the proto: the §7 services, messages and tags; MCP op schemas `src/bin/tool_support/graph_mcp.rs:584-669` and a KV forwarder beside `graph_proxy.rs:1-9`; parsers `OpIn` (`graph_json.rs:208`), `BatchIn` (`src/server/graph.rs:701-705`), the C ABI (`bindings/c/src/graph.rs:191`), `bindings/node/index.d.ts:274-285` and the `vlite-graph-cli` batch JSON |
| X3 | `src/server.rs`, `src/bin/tool_support/graph_json.rs`, `src/server/grpc.rs` | Arms and stable codes for `InvalidKvKey` (422 `invalid_input`), `KvSlotsExhausted` (409 `kv_slots_exhausted`) and `NotKvCollection` (409 `not_kv_collection`), beside the fallback `error => internal(error)` (`src/server.rs:1624`) and `graph_code` (`src/server.rs:1631-1654`, fallback at `:1654`); the tool layer's `error_code` (`graph_json.rs:26-72`); the gRPC table (`src/server/grpc.rs:181-197`) |
| X4 | `src/server/admin.rs`, `docs/CAPABILITIES.md` | The `features` object and `routes` array (`src/server/admin.rs:410-423`) gain the proposed flags `kv`, `kv_leases` and `graph_ttl` and the new routes, which clients read before using a surface (§6); `docs/CAPABILITIES.md` documents them (it already covers `features.tenant_lifecycle`, `docs/CAPABILITIES.md:195`) |
| X5 | `docs/GRAPH.md`, `docs/GRPC.md`, `docs/API.md`, `docs/MCP.md`, `docs/WEBHOOKS.md`, `docs/TOOLS.md`, `docs/SECURITY.md`, `docs/STORAGE-FEATURES.md` | Replace "embedded Rust API only" (`GRAPH.md:60-62`), "does not report deadlines or removals yet" (`GRAPH.md:93-96`), the graph-feed paragraph (`GRPC.md:86-90`) and "REST, gRPC and bindings come after the OpenAPI review" (`STORAGE-FEATURES.md:283`) with what the shipped slice supports; the other files gain the routes, tools, webhook option, CLI commands and the scope and tenancy rules of §10 |
| X6 | `site/api.html`, `tests/api_reference.py` | New rows and `data-api` keys beside the existing Rust KV rows (`site/api.html:1062-1074`); the extractor (`tests/api_reference.py:9-23`) gains the `http-py`, `http-js`, `dotnet` and `cli` families; `grpc:` keys stay unqualified with unique short names under wire conventions decision 16 (proposed), §8 |

Each decision records the recommended option; the owner approves it before
code. Decisions (e), (g) and (h) touch wire shared with other documents and stay
proposals pending owner ratification; every row below remains an open proposal,
not a ratified choice. **Withdrawn:** the former separate-service
placement and boolean/mode feed alternatives in (e), (g) and (m) are replaced
by the proposals in wire conventions decisions 1, 2 and 8. Wire conventions
decision 9 (proposed) limits
(n)'s chunks to TTL reclaim runs. The appended references name the wire
conventions proposals behind
the text above without rewriting those original rows.

| Decision (section) | Options | Recorded choice |
|---|---|---|
| (a) KV and lease durability (§4, §5) | All writes follow `--durability` / lease writes always sync and the route refuses when it cannot / sync every KV write | Lease writes always sync and refuse with `lease_unavailable` under `none`, read-only, replica or shared opens; ordinary KV follows the configured durability, and docs and OpenAPI say so |
| (b) Lease ledger placement (§5) | Record revisions / ordinary KV records / a new file section behind a capability flag with the P2 converter / an external allocator | A storage-format change delivered by `gap2-rust-lease-ledger` with the converter; revisions alone fail LEASE-001 |
| (c) Fencing after restore, promotion, point-in-time output, checkpoint restore or copy (§5) | Fail closed / an external non-rollback floor / build recovery | Fail closed (`fencing_recovery_required`): a fresh domain and explicit re-enrollment (`scale-out.md:28,357`) |
| (d) KV credential scopes (§10) | Reuse `read`/`write` / new `kv_read`/`kv_write` / reuse graph scopes | Reuse `read`/`write`, because a store naming an unknown scope fails to load in an older build (`src/server/credentials.rs:41-42`); dedicated scopes stay a future option |
| (e) Graph TTL placement and the 19-22 amendment (§7) | A narrow `GraphTtl` service plus an amendment of `serving-contract.md:206` / wait for the full Graph service / another placement | Narrow `GraphTtl` plus the amendment (ledger X1); mixed create and TTL batches wait for the full Graph schema review; formerly proposed placement, **Withdrawn** as explained above. Wire conventions, decision 1 (proposed); Wire conventions, decision 2 (proposed) |
| (f) DELETE shape (§4) | JSON body (DELETE has no defined body semantics) / query parameters / POST `.../delete` | Query parameters and the existing 200 `{"deleted"}` shape (`src/openapi.json:115`) |
| (g) TTL-mode feed cursor (§6) | Keep `vlc1` with an out-of-band mode flag / `vlc2` binding lineage, mode, sequence and ordinal / a separate endpoint | `vlc2` with `cursor_mode_mismatch`; formerly proposed mode binding, **Withdrawn** in favor of set binding above. Wire conventions, decision 8 (proposed) |
| (h) Size errors (§3, §10) | New size-specific error codes / reuse existing codes | Reuse 413 `invalid_json` and 422 `invalid_input`; gRPC size violations stay tonic's `OUT_OF_RANGE` |
| (i) C ABI and WASM scope (§9) | Embedded KV and leases in every binding / C ABI graph TTL only / none | C ABI graph TTL only; WASM explicitly unsupported; embedded KV and leases wait for the Rust slices; HTTP clients and the CLI get full parity |
| (j) Restart holders (§5) | Preserve live grants with a durable clock protocol / invalidate all at recovery | Invalidate all; document the availability tradeoff |
| (k) Lease quotas (§10) | A fixed universal cap / an explicit configured per-tenant quota / unlimited | An explicit finite quota; activation is rejected until configured; allocator history is never evicted |
| (l) Lease holder identity (§5) | Transferable token / principal-bound / an admin takeover API | Principal-bound; no takeover API |
| (m) Feed compatibility (§6) | Change the default decoder / opt-in `graph_ttl` | Opt-in; the default feed is unchanged; formerly proposed boolean, **Withdrawn** in favor of the set above. Wire conventions, decision 8 (proposed) |
| (n) Oversized reclaim feed (§6) | Truncate / fail permanently / deterministic resumable chunks | Chunks; independent cursor and replay acceptance is mandatory. Wire conventions, decision 9 (proposed) |
| (o) Wire numbers (§3) | JSON numbers / canonical decimal strings | Decimal strings for every new u64; existing numeric inputs only where already public |
| (p) KV batch (§2, §12) | Simulate with sequential writes / a storage transaction API first / omit | Omit; a future batch needs independent atomicity and schema review |

Not decided here: the ledger byte layout, framing and replay; restore
provenance detection; the converter; the lease-quota configuration surface;
and any future extractor families (ledger X6); RPC short-name uniqueness follows
wire conventions decision 16 (proposed). None permits
weakening the normative contract or shipping a partial lease API. The owner
resolves or explicitly defers each decision before the affected surface ships;
this document claims no implementation or release.
