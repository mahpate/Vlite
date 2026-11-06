# Key-value store

The key-value store keeps byte values under namespaced string keys, with no vector, inside an ordinary one-dimension collection. It offers compare-and-set on revisions, optional expiry and ordered prefix listing from Rust, REST, gRPC, the Python, JavaScript and .NET clients, and a local shell and MCP server.

This guide explains how to use it and what it does not do. The reference text of each surface stays in [REST](API.md#key-value-entries), [gRPC](GRPC.md#key-value-entries), [storage features](STORAGE-FEATURES.md#key-value-store), the [MCP tools](MCP.md#local-key-value-tools), the [shell commands](TOOLS.md#local-key-value-shell-commands) and [Key-value permissions](SECURITY.md#key-value-permissions).

## Concepts

- **Collection.** An entry is stored in an ordinary collection with one dimension. Use a dedicated collection for key-value data: vector, import and graph operations on the same collection can overwrite entries. The Rust `Database::kv_store` creates a missing one-dimension collection (format 4 or newer). Every other surface needs an existing collection, never creates one and answers 409 `not_kv_collection` for a collection with another dimension.
- **Namespace and key.** UTF-8 strings that are not normalized: a namespace is 1 to 255 bytes and a key 1 to 1,024 bytes. Keys sort by unsigned UTF-8 bytes. A namespace partitions keys inside one collection. It is not an authorization boundary: anyone who can read or write the collection can read or write every namespace in it.
- **Value.** Bytes, from 0 to 1,047,290. Over JSON they travel as canonical padded standard base64.
- **Version.** Each entry has a version, which is the commit sequence of its record. It is a decimal string over REST, MCP and the shell, and a `uint64` in gRPC and Rust. A put or delete can require the entry's current version (compare-and-set).
- **Expiry.** An entry can carry an absolute deadline in Unix milliseconds (`expires_at_ms`). A due entry reads as absent. Expiry needs a format 5 file.
- **Storage.** There is no file format change. An entry is an ordinary record. Its record ID comes from a stable hash of the namespace and key, and the key may occupy any of 16 consecutive IDs (its *slots*). Ordinary record quotas count entries.

## Choose a surface

| Surface | Runs against | Creates the collection | Operations |
|---|---|---|---|
| Rust: `Database::kv_store`, `kv_view` | a file you open | yes (`kv_store`) | get, put, delete, list |
| REST | `vlite-server` | no | `GET`, `PUT`, `DELETE` on `.../kv/entry`; `GET` on `.../kv/entries` |
| gRPC | `vlite-server` built with `grpc` | no | `vlite.v1.Kv`: `GetKvEntry`, `PutKvEntry`, `DeleteKvEntry`, `ListKvEntries` |
| Python, JavaScript, .NET clients | `vlite-server` | no | `kv_get`, `kv_put`, `kv_delete`, `kv_list`; `kvGet` and so on; `KvGetAsync` and so on |
| `vlite-shell --db PATH` | a local file | no | `kv-get`, `kv-put`, `kv-delete`, `kv-list` |
| `vlite-mcp --db PATH` | a local file | no | `vlite_kv_get`, `vlite_kv_put`, `vlite_kv_delete`, `vlite_kv_list` |
| Dashboard **Key-value** workspace | `vlite-server` | no | the REST routes; see the [dashboard guide](CONSOLE.md#key-value) |

The shell and MCP tools open the file themselves, so stop `vlite-server` first, and they are not forwarded by `vlite-mcp --server`. There are no lease operations on any surface.

## Start with REST

Create a one-dimension collection once (this needs the admin token or a credential that holds the `collections` operation on `*`), then write, read, list and delete. Put the server address and a token in `VLITE_URL` and `VLITE_TOKEN`.

```sh
curl -sS -X POST "$VLITE_URL/v1/collections" \
  -H "Authorization: Bearer $VLITE_TOKEN" -H "Content-Type: application/json" \
  -d '{"name":"cache","dimensions":1}'

curl -sS -X PUT "$VLITE_URL/v1/collections/cache/kv/entry" \
  -H "Authorization: Bearer $VLITE_TOKEN" -H "Content-Type: application/json" \
  -d '{"namespace":"app","key":"greeting","value_base64":"aGVsbG8=","expected":"absent"}'

curl -sS "$VLITE_URL/v1/collections/cache/kv/entry?namespace=app&key=greeting" \
  -H "Authorization: Bearer $VLITE_TOKEN"

curl -sS "$VLITE_URL/v1/collections/cache/kv/entries?namespace=app&prefix=gre&limit=100" \
  -H "Authorization: Bearer $VLITE_TOKEN"

curl -sS -X DELETE "$VLITE_URL/v1/collections/cache/kv/entry?namespace=app&key=greeting&expected=any" \
  -H "Authorization: Bearer $VLITE_TOKEN"
```

The put answers `{"version":"…","expires_at_ms":null}`, the get `{"key":"greeting","value_base64":"aGVsbG8=","version":"…","expires_at_ms":null}`, the list `{"items":[…],"next_after":null}` and the delete `{"deleted":true}`. The put with `"expected":"absent"` creates the key and fails with 412 if it already exists.

| Route | Scope | Result (HTTP 200) |
|---|---|---|
| `PUT /v1/collections/{collection}/kv/entry` | `write` | `version`, `expires_at_ms` |
| `GET /v1/collections/{collection}/kv/entry?namespace=n&key=k` | `read` | `key`, `value_base64`, `version`, `expires_at_ms` |
| `DELETE /v1/collections/{collection}/kv/entry?namespace=n&key=k[&expected=…]` | `write` | `deleted` |
| `GET /v1/collections/{collection}/kv/entries?namespace=n[&prefix=…][&after=…][&limit=…]` | `read` | `items`, `next_after` |

Requests are strict. A put takes no query parameters. Unknown or duplicate JSON fields and wrong types answer 422 `invalid_json`, bound or encoding violations answer 422 `invalid_input`, and unknown or duplicate query parameters answer 400 `invalid_query`. Versions and deadlines are canonical `uint64` decimal strings. Query strings use form decoding, so `+` is a space and a literal plus is sent as `%2B`. `HEAD` on the two `GET` routes behaves like `GET` without a body. The request body limit is 8 MiB.

## Compare-and-set with revisions

`expected` states what the entry must look like for the write to happen. The check and the write are one atomic step.

| `expected` | The write happens when |
|---|---|
| `"any"` (the default) | always |
| `"absent"` | the key has no live entry (an expired entry counts as absent) |
| `{"revision":"42"}` over REST, `expected=42` on a delete | the live entry's version is exactly 42 |

A mismatch answers 412 `precondition_failed` and writes nothing. A revision condition on a key that is missing or due answers 412 too. Over REST and gRPC the message is always `revision precondition failed`, so a caller that may write but not read cannot learn whether a key exists or what its revision is. Revision `"0"` is valid input but never matches a live entry. A delete of a missing key with `any` or `absent` succeeds with `deleted: false`.

A put returns the new version, so a read-modify-write loop does not need a second read:

1. Get the entry and remember its `version`.
2. Compute the new value and put it with `expected` set to that version.
3. On 412, nothing was written: go back to step 1.

A transport error is different from a 412: the write may have committed. Read the entry to find out before you decide anything (see [Durability](#durability)).

Versions are commit sequences. They increase for a key within one file lineage, so they work as compare-and-set tokens, but compaction, upgrade and restore rewrite revisions. Read an entry again after any of those before you use an old version. A directory store checkpoint keeps every live record's revision, so versions stay valid across it.

## Expiry

`expires_at_ms` is an absolute Unix-millisecond deadline, sent as a decimal string. A put without it, or with `null`, clears the entry's expiry, so pass the deadline again whenever you refresh a value. Due means `deadline <= now`, judged on the server's clock when the call is served, so a client whose clock is wrong changes how long an entry lives. A put with a deadline that is already due succeeds, and the entry then reads as absent. There is no call that only extends a deadline: put the value again, with `expected` set to the version you read if another writer may be active.

An expired entry stays in the file until it is replaced, deleted or compacted away, and it still counts toward record quotas. A later put of another key can reuse its slot. Expiry needs format 5: on a format 4 file a put with a deadline answers 409 `upgrade_required`, and nothing is upgraded for you.

The ordinary record routes treat a due expiry differently from this: `PUT /v1/collections/{collection}/vectors/{id}` refuses an `expires_at_ms` that is not later than the server clock, and a `ttl_ms` of 0, with 422 `invalid_input` (see [Record expiry and metadata](API.md#record-expiry-and-metadata)). The key-value routes accept it by contract.

## Listing, paging and its cost

A list returns the live entries of one namespace whose key starts with `prefix` (default empty, at most 1,024 bytes), in key order, strictly after `after` when given, at most `limit` (1 to 10,000, default 100). A page is the largest prefix of that result that fits both `limit` and 16,777,216 bytes of compact JSON, including the `{"items":[…],"next_after":…}` envelope. `next_after` is the last returned key when another live matching entry exists, and `null` otherwise. To continue, pass it as `after`. Each key appears once, as `get` returns it.

There is no snapshot across pages: one page uses one expiry clock, and entries written or expired between two calls can appear or vanish. Follow `next_after` until it is `null`, even after a short page.

A list reads the whole collection. Its cost grows with the number of entries in the collection, not with `limit`, and heavy listing can slow writers. Since 0.7.1 the memory a list holds is bounded by `limit` and the 16 MiB page, not by the number of entries: the scan keeps only the `limit + 1` smallest candidate keys, and a REST, gRPC, MCP or shell page is built from one scan that encodes only the entries it returns. Keep large key spaces in their own collections and list by a narrow prefix.

In Rust, `KvView::list` copies every value it returns: 10,000 values of just under 1 MiB are roughly 10 GB, so use a smaller `limit` when values are large. `KvView::visit_at` lends each entry to a callback as a `KvRef` instead (only the library builds one, and a later release may add fields), with `has_next` telling whether the listing continues, and stops when the callback returns `ControlFlow::Break(())`:

```rust
use std::{ops::ControlFlow, time::{SystemTime, UNIX_EPOCH}};
use vlite::Database;

fn print_keys(db: &Database) -> vlite::Result<()> {
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    db.kv_view("cache")?.visit_at("app", "job/", None, 100, now_ms, |entry| {
        println!("{} is at version {} ({} bytes)", entry.key, entry.version, entry.value.len());
        ControlFlow::Continue(())
    })
}
```
<!-- TODO-VERIFY: the Rust example above is written from the `visit_at` signature in src/kv.rs and site/api.html and was not compiled (cargo is not run for this docs work). -->

## Limits

| Item | Limit |
|---|---|
| Namespace | 1 to 255 UTF-8 bytes |
| Key | 1 to 1,024 UTF-8 bytes |
| List `prefix` | at most 1,024 UTF-8 bytes |
| Value | 0 to 1,047,290 bytes (1,396,388 base64 characters) |
| List `limit` | 1 to 10,000 (default 100) |
| List page | 16,777,216 bytes of compact JSON |
| Request body | 8 MiB |
| Slots per key | 16 record IDs |
| gRPC | 8 MiB request, 16 MiB response; a client needs a receive limit of at least 16 MiB + 1 KiB for `ListKvEntries` and a send limit above 1 MiB |
| `vlite-shell` command line | 16 MiB |
| `vlite-mcp` request line | 128 MiB (134,217,728 bytes) |

A line over the shell or MCP limit is refused (`invalid_input: line exceeds 16777216 bytes` in the shell; JSON-RPC error -32600 with a null `id` in MCP, after which the session goes on). A key-value call is far below both limits.

## Slot exhaustion

A key hashes to 16 consecutive record IDs. If all 16 hold live entries of other keys or other records, a put for that key answers 409 `kv_slots_exhausted` (gRPC `FAILED_PRECONDITION`; the shell and MCP prefix `kv_slots_exhausted:`). A slot that holds an expired entry of another key is reused, and a record that is not a key-value entry is never overwritten. Anyone who can write the collection can place ordinary records at a key's IDs, so give `write` on a key-value collection only to principals you trust with its keys.

The JSON metrics carry `kv_slots_exhausted_total` and Prometheus carries `vlite_kv_slots_exhausted_total`: the number of key-value puts refused this way, across all credentials (a REST request whose client disconnected before the answer was produced is not counted; a gRPC call is counted even if its client cancels). A rise that nobody expects points at records planted over a key's IDs. The slots are the record IDs from `vlite::kv::first_slot(namespace, key)` (Rust), wrapping, so freeing a key means removing the other records at those IDs. Using another key also works.
<!-- TODO-VERIFY: "freeing a key means removing the other records at those IDs" is derived from the slot probe in src/kv.rs, not from a documented procedure. -->

## Durability

Key-value writes follow the server's configured durability. With `full`, a put or delete is synced before it is acknowledged. With `periodic` (the default, 60 s), an OS crash or power loss can lose up to about one flush interval. With `none`, data is memory-only. There is no per-call sync, no automatic retry and no lease guarantee. A successful call is acknowledged after the commit. A disconnect or a `commit_indeterminate` answer (503 over REST, `UNKNOWN` over gRPC) does not prove that a write failed: read the entry, and do not retry a put blindly. See [periodic durability](STORAGE-FEATURES.md#periodic-lazy-durability).

## Revisions are not fencing tokens

A version is a commit sequence inside one file lineage. It rises with every write to the file, and it changes after compaction, upgrade and restore. It is a compare-and-set token, not a durable lease-fencing token: entry revisions do not establish durable, never-reused fencing domains, so a version cannot stop a slow holder's later side effects. Leases and a fencing-token ledger are not implemented; they are a design only (see [Not included in 0.7.1](RELEASE-0.7.1.md#not-included-in-this-release)). A recipe that needs mutual exclusion must stay safe when two workers briefly believe they hold the same claim.

## Use it from a client

The Python, JavaScript and .NET clients call the REST routes. They need a server with the key-value routes and an existing, dedicated one-dimension collection (`cache` below). Values are bytes, and versions and deadlines are lossless 64-bit integers. Omitted or null expiry clears the TTL. A 404 `kv_not_found` is an ordinary error, and only an unknown route (404 `not_found`) becomes an unsupported-operation error. A timeout or cancellation may hide a committed write, and nothing is retried.

**Python** (`vlite-client`):

```python
import os
from vlite_client import Client

client = Client(os.environ["VLITE_URL"], os.environ["VLITE_AUTH_TOKEN"])
result = client.kv_put(collection="cache", namespace="app", key="hello", value=b"world", expected="any")
entry = client.kv_get(collection="cache", namespace="app", key="hello")
print(entry["value"], result["version"])
print(client.kv_list(collection="cache", namespace="app")["items"])
print(client.kv_delete(collection="cache", namespace="app", key="hello", expected={"revision": result["version"]}))
```

**JavaScript** (`@vlite/client`, an ESM script):

```javascript
import { Client } from "@vlite/client";

const client = new Client(process.env.VLITE_URL, process.env.VLITE_AUTH_TOKEN);
const result = await client.kvPut({ collection: "cache", namespace: "app", key: "hello",
  value: new TextEncoder().encode("world"), expected: "any" });
console.log(await client.kvGet({ collection: "cache", namespace: "app", key: "hello" }));
console.log(await client.kvList({ collection: "cache", namespace: "app" }));
console.log(await client.kvDelete({ collection: "cache", namespace: "app", key: "hello",
  expected: { revision: result.version } }));
```

**.NET** (`VLite.Client`, a .NET 8 console application):

```csharp
using VLite;

using var client = new Client(Environment.GetEnvironmentVariable("VLITE_URL")!,
                              Environment.GetEnvironmentVariable("VLITE_AUTH_TOKEN")!);
var result = await client.KvPutAsync("cache", "app", "hello", new byte[] { 1, 2 }, new Expected.Any());
var entry = await client.KvGetAsync("cache", "app", "hello");
Console.WriteLine(entry.Version);
var page = await client.KvListAsync("cache", "app");
Console.WriteLine(page.Items.Length);
Console.WriteLine(await client.KvDeleteAsync("cache", "app", "hello", new Expected.Revision(result.Version)));
```

Python `Expected` is `"any"`, `"absent"` or `{"revision": int}`. JavaScript `Expected` is `"any"`, `"absent"` or `{ revision: bigint }`, and versions are `bigint`, never `Number`. .NET uses `Expected.Any`, `Expected.Absent` or `Expected.Revision(ulong)`. See the [Python](../clients/python/README.md#key-value-operations), [JavaScript](../clients/javascript/README.md#key-value-operations) and [.NET](../clients/README.md#net-key-value-operations) client guides.

## Use it from the shell and MCP

`vlite-shell --db PATH` and `vlite-mcp --db PATH` work on a local file, so stop `vlite-server` first. Each takes the same fields as REST, with a required `collection` that must already exist and have one dimension. `vlite-shell` takes one JSON object per line:

```text
kv-put {"collection":"default","namespace":"cache","key":"a","value_base64":"AAEC","expected":"absent"}
kv-get {"collection":"default","namespace":"cache","key":"a"}
kv-list {"collection":"default","namespace":"cache","prefix":"","limit":100}
kv-delete {"collection":"default","namespace":"cache","key":"a","expected":"any"}
```

The example uses the `default` collection, which therefore has to be a one-dimension database; use the name of your own key-value collection otherwise. `vlite-mcp` lists `vlite_kv_get` and `vlite_kv_list`, and with `--writable` also `vlite_kv_put` and `vlite_kv_delete`; a `tools/call` takes the same fields as the shell command:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"vlite_kv_put","arguments":{"collection":"cache","namespace":"app","key":"a","value_base64":"AAEC","expected":"absent"}}}
```

Failures print `error: <code>: <message>` in the shell (exit code 1) and set `isError: true` in MCP, with the code first: `kv_not_found`, `collection_not_found`, `not_kv_collection`, `precondition_failed`, `kv_slots_exhausted`, `upgrade_required`, `invalid_request` for unknown or duplicate fields and wrong types, `invalid_input` for bounds, and `read_only` for a write in a read-only session. The shell and MCP tools have no lease commands.

## Errors

| Situation | REST | gRPC | Shell and MCP code |
|---|---|---|---|
| Entry absent or due (get) | 404 `kv_not_found` | `NOT_FOUND` | `kv_not_found` |
| Collection does not exist | 404 `collection_not_found` | `NOT_FOUND` | `collection_not_found` |
| Collection is not one-dimension | 409 `not_kv_collection` | `FAILED_PRECONDITION` | `not_kv_collection` |
| All 16 slots occupied | 409 `kv_slots_exhausted` | `FAILED_PRECONDITION` | `kv_slots_exhausted` |
| Expiry on a format 4 file | 409 `upgrade_required` | `FAILED_PRECONDITION` | `upgrade_required` |
| `expected` does not match | 412 `precondition_failed` | `ABORTED` | `precondition_failed` |
| Bound or encoding violation | 422 `invalid_input` | `INVALID_ARGUMENT` | `invalid_input` |
| Unknown or duplicate JSON field, wrong type | 422 `invalid_json` | | `invalid_request` |
| Unknown or duplicate query parameter | 400 `invalid_query` | | |
| Scope, tenant or collection binding refused | 403 `forbidden_scope` | `PERMISSION_DENIED` | |
| Read-only database or token | 403 `read_only` | `PERMISSION_DENIED` | `read_only` |
| Request rate or live-vector quota | 429 `rate_limited` or `quota_exceeded` | `RESOURCE_EXHAUSTED` | |
| Commit outcome unknown | 503 `commit_indeterminate` | `UNKNOWN` | |

gRPC errors carry `vlite-error-code` and `x-request-id`. Size errors from the transport are `OUT_OF_RANGE` without an application code.

## Recipes

Each recipe uses the Python client from above and a dedicated collection. They show the patterns that the store can support. None of them is a lock manager.

### A counter

A compare-and-set loop makes the increment atomic for one key. It retries only on 412, which means nothing was written. A transport error is not caught: the outcome is unknown, so it propagates.

```python
import os
from vlite_client import Client, VLiteError

client = Client(os.environ["VLITE_URL"], os.environ["VLITE_AUTH_TOKEN"])

def increment(collection: str, namespace: str, key: str) -> int:
    while True:
        try:
            entry = client.kv_get(collection=collection, namespace=namespace, key=key)
            expected, count = {"revision": entry["version"]}, int(entry["value"]) + 1
        except VLiteError as error:
            if error.code != "kv_not_found":
                raise
            expected, count = "absent", 1
        try:
            client.kv_put(collection=collection, namespace=namespace, key=key,
                          value=str(count).encode(), expected=expected)
            return count
        except VLiteError as error:
            if error.code != "precondition_failed":
                raise

print(increment("cache", "stats", "page-views"))
```

It protects one key. There are no multi-key transactions, so two counters cannot be changed together.

### A cache with a time to live

Give every put a deadline, and treat `kv_not_found` as a miss. A refreshed value needs its deadline again, because a put without one clears the TTL.

```python
import os
import time
from vlite_client import Client, VLiteError

client = Client(os.environ["VLITE_URL"], os.environ["VLITE_AUTH_TOKEN"])
ttl_ms = 60_000

client.kv_put(collection="cache", namespace="pages", key="home", value=b"<html>home</html>",
              expires_at_ms=int(time.time() * 1000) + ttl_ms)
try:
    page = client.kv_get(collection="cache", namespace="pages", key="home")["value"]
except VLiteError as error:
    if error.code != "kv_not_found":
        raise
    page = None  # never written, or expired
print(page)
```

The server's clock judges the deadline, so keep client clocks in step with it. Expired entries are hidden at once but stay in the file until they are replaced, deleted or compacted away, and they count toward record quotas. Plan the collection's size for that, and delete entries you know are dead.

### A claim without leases, and its caveats

An absent-only put with a deadline is a time-limited claim on a key: exactly one of several workers succeeds, and the others get 412.

```python
import os
import time
import uuid
from vlite_client import Client, VLiteError

client = Client(os.environ["VLITE_URL"], os.environ["VLITE_AUTH_TOKEN"])
worker = uuid.uuid4().hex.encode()

def claim(job: str, seconds: int = 30):
    deadline = int(time.time() * 1000) + seconds * 1000
    try:
        return client.kv_put(collection="pool", namespace="claims", key=job, value=worker,
                             expected="absent", expires_at_ms=deadline)["version"]
    except VLiteError as error:
        if error.code == "precondition_failed":
            return None  # another worker holds a live claim
        raise

def release(job: str, version: int) -> bool:
    try:
        return client.kv_delete(collection="pool", namespace="claims", key=job,
                                expected={"revision": version})
    except VLiteError as error:
        if error.code == "precondition_failed":
            return False  # the claim expired and someone else took it
        raise
```

To extend a claim, put again with `expected={"revision": version}` and a new deadline: the put returns the new version. The caveats are part of the design, not bugs to work around:

- The claim is advisory and time-based. When the deadline passes, another worker can claim the job while the first worker is still running. The store cannot stop the first worker's side effects.
- The version is not a fencing token. Another system cannot compare versions to refuse a stale worker, because versions change after compaction, upgrade and restore (see [Revisions are not fencing tokens](#revisions-are-not-fencing-tokens)).
- If a put ends in a transport error, the claim may exist. Read the entry and compare its value with your worker ID before you conclude anything. Do not retry the put: it would fail with 412 if the first one committed.
- Do not claim with a deadline that is already due: the put succeeds and the claim is gone at once.
- Make the work idempotent, or record its completion in the same system that takes the side effect.

## Security and audit

Get and list need the collection's `read` scope, and put and delete need `write`; the same scopes apply to the `vlite.v1.Kv` RPCs. Graph scopes do not grant key-value access, read-only credentials cannot mutate, and tenant-local names and collection bindings are checked before any lookup. Put sensitive data in dedicated collections and bind credentials narrowly: vector, graph-attachment and import permissions can also expose, overwrite, delete or expire key-value records.

Put and delete are audited as `kv_put` and `kv_delete` events with the principal, a stable [actor](SECURITY.md#audit-actors), the collection, the request ID and the outcome, and for a put the deadline, never the namespace, key or value. A request that fails records the collection as `(invalid)` and still records the actor. A request that is rejected while it is parsed is not audited. See [Key-value permissions](SECURITY.md#key-value-permissions).
