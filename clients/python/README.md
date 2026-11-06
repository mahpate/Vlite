# VLite Python client

Synchronous, typed REST client for Python 3.11+ with no runtime dependencies.
For application use, install the published
[vlite-client package from PyPI](https://pypi.org/project/vlite-client/):

```text
python -m pip install vlite-client
```

For contributor development from a
VLite checkout, install the working tree with
`python -m pip install ./clients/python`.

See the [client guide](https://github.com/mahpate/Vlite/blob/main/clients/README.md)
for authentication, examples, supported operations and uncertain-write handling.
This package does not automatically retry requests or follow redirects.

`embed_text(text, provider="hashing")` returns an `EmbedResponse` containing
`vector`, `dimensions` and `provider`. The provider may also be legacy `http` or
an admin-authorized alias (1–256 ASCII letters, digits, `.`, `_` or `-`).

`Search` accepts optional `rerank: RerankOptions` with `mode` (`exact`, `mmr` or
`custom`), optional `candidates`, the wire key `"lambda"` for MMR, and `provider`
(required by the server for custom mode). Requests are forwarded unchanged;
the server validates mode-specific fields and candidate bounds. For custom
mode, `k <= candidates <= 1000`, defaulting to `max(k, 50)`.

Custom results may carry a `rerank_score` on each `Hit`/`HybridHit`, preserving
the original metric `distance`. `search_detailed` also retains optional
`ranking: Ranking` (`provider` and `order: "score_desc_id_asc"`); this orders
custom scores descending, then numeric IDs ascending. `search` still returns
only hits. Older responses without these optional fields remain compatible.
Provider errors propagate without fallback. Selecting an alias requires a
server configured to support it; the client does not configure providers.

Private/unreleased, owner-approved REST candidate: HTTP 409
`provider_snapshot_conflict` carries a sanitized error and no `Retry-After`.
The client propagates it unchanged as `VLiteError`, without retry or fallback.
If the caller elects to retry, it must retry the whole request.

## Key-value operations

The KV REST surface requires a server with KV routes and an existing,
dedicated one-dimensional collection (`cache` below). Namespaces are application
partitions, not authorization boundaries. Run with `VLITE_URL` and
`VLITE_AUTH_TOKEN` set:

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

`Expected` is `"any"`, `"absent"` or `{"revision": int}`. Values are `bytes`
(at most 1,047,290 bytes); versions and `expires_at_ms` are lossless `int`s.
Omitted/`None` expiry clears TTL; deadlines are absolute Unix milliseconds.
List uses an exclusive `after` key and returns `next_after` (no snapshot across
pages). Missing/due get raises `VLiteError` with `kv_not_found`; only an unknown
route (404 `not_found`) raises `UnsupportedError`. No fallback or automatic retry
occurs. A timeout may hide a committed mutation; re-read before deciding what
to do. Acknowledgements sync only with server `--durability full`.

`kv_get_many` and `kv_batch` take up to 1,000 keys or operations of one collection
in a single request. A server without them answers the admin token 404
`not_found`, which raises `UnsupportedError`, and every other credential 403
(`read_only` or `forbidden_scope`), which raises `VLiteError`, because its
permission check runs before it matches a route. To find out beforehand, look for
`/v1/collections/{collection}/kv/batch` and
`/v1/collections/{collection}/kv/entries/get` in the `routes` of
`GET /v1/capabilities`, which every credential may call; `features.kv_batch` is
not that test, because a format 3 or 4 file serves both routes with the flag
false. The client never falls back to single calls and never retries:

```python
keys = [{"namespace": "app", "key": "a"}, {"namespace": "app", "key": "b"}]
read = client.kv_get_many(collection="cache", keys=keys)
print([None if item is None else item["value"] for item in read["items"]], read["next_index"])

done = client.kv_batch(collection="cache", operations=[
    {"op": "put", "namespace": "app", "key": "a", "value": b"1", "expected": "absent"},
    {"op": "delete", "namespace": "app", "key": "b", "expected": {"revision": 42}},
])
print(done)  # one result per operation, in order: {"version": int} or {"deleted": bool}
```

`kv_get_many` takes `KvKey` dicts (`{"namespace", "key"}`, a key may repeat) and
returns a `KvGetManyResult` whose `items` follow the order of `keys`; `None` marks a
key that is absent or due, which is not an error. A call whose answer would pass the
server's 16 MiB response cap returns only a prefix of the keys, with `next_index` set
to the first key not read (`None` when every key was read). The client does not
continue a partial result for you: loop on `next_index` yourself. Calls share no
snapshot, so a later call sees the state at its own time:

```python
pending, entries = keys, []
while pending:
    page = client.kv_get_many(collection="cache", keys=pending)
    entries += page["items"]
    pending = [] if page["next_index"] is None else pending[page["next_index"]:]
```

A `kv_batch` operation is a `KvBatchPut` or a `KvBatchDelete`:
`{"op": "put", "namespace": str, "key": str, "value": bytes, "expected": Expected}`
or `{"op": "delete", "namespace": str, "key": str, "expected": Expected}`, where
`expected` is optional and defaults to `"any"`. The batch commits every operation or
none. The results answer the operations in order: a `KvBatchPutResult`
(`{"version": int}`) for a put and a `KvBatchDeleteResult` (`{"deleted": bool}`) for a
delete. All puts of one batch share one version, which is a valid
`{"revision": ...}` for each of their keys. The first operation that cannot run (a
failed `expected` is 412 `precondition_failed`, no free slot is 409
`kv_slots_exhausted`) fails the whole batch, and nothing is written. A batch names
each `(namespace, key)` at most once, holds at most 1,000 operations and carries at
most 4,194,304 value bytes in total. The client checks these bounds and rejects any
unknown field with `ValueError` before it sends anything. A batch put has no
`expires_at_ms`: like a put without one, it clears the deadline of the entry it
replaces. After a `TransportError` or a 503 `commit_indeterminate` the batch is applied
entirely or not at all, but which is unknown: read the keys back with `kv_get_many`
before deciding anything, and do not retry blindly.

## Agent memory

The agent-memory REST surface needs a server with a provisioned memory
binding (`notes` below). `client.memory(binding)` makes no request and returns a
facade with nine methods; each takes one `TypedDict` request and returns the full
typed response, including an HTTP 202 pending result. Run with `VLITE_URL` and
`VLITE_AUTH_TOKEN` set:

```python
import os
from vlite_client import Client, UnsupportedError, VLiteError

client = Client(os.environ["VLITE_URL"], os.environ["VLITE_AUTH_TOKEN"])
memory = client.memory("notes")
scope = {"agent": "assistant", "user": "ada"}
done = memory.remember({
    "scope": scope,
    "idempotency_key": "turn-42",  # required; never generated or retried
    "items": [{"id": "e1", "kind": "entity", "content": "Ada", "observed_at_ms": 90,
               "source": {"source_id": "chat", "source_kind": "caller"}}],
})
print(done["receipt"]["sequence"], done["outcomes"])
for view in memory.recall({"scope": scope, "text": "Ada", "limit": 5})["memories"]:
    print(view["id"], view["scores"]["S"])
try:
    memory.find_facts({"scope": scope, "slot": {"subject_id": "e1", "predicate": "role", "slot_key": "main"}})
except VLiteError as error:  # a refusal is never hidden
    print(error.status, error.code, error.request_id, error.details)
except UnsupportedError:  # 404 not_found: the server has no memory routes
    pass
```

Text and vector recall find only memories that hold an evidence record, so the `text` recall above
returns nothing on a new binding, and a binding without an evidence collection refuses it with 409
`memory_capability_unavailable` (`details["feature"]` `evidence_recall`). Recall with
`"entity_seeds": ["e1"]` to read back that entity and the memories it references.

The methods are `remember`, `recall`, `find_facts`, `correct_facts`, `consolidate`,
`forget`, `get_receipt`, `get_erasure` and `resume_erasure`; types carry the
contract's names (`MemoryRememberRequest`, `MemoryTarget`, `MemoryConsolidateResponse`
and so on). `MemoryU64`/`MemoryI64` values in responses are canonical decimal
strings; requests accept the same string or an exact in-range `int` and send the
canonical string. Unknown request fields raise `ValueError` and are never sent;
optional fields are omitted (only interval endpoints may be `None`). `consolidate`
returns a `MemoryMutationResponse` or, with `dry_run`, a `MemoryPlanResponse`.

`VLiteError.details` carries the optional structured context (`field`, `limit`,
`requested`, `feature`, `category`). Only 404 `not_found` raises `UnsupportedError`;
404 `memory_not_found`, `receipt_not_found` and `erasure_not_found` are ordinary
errors. The first server slice (provisional) serves `remember` for kinds `entity`
and `episodic`, `recall`, `consolidate` (retain only, commit only), `forget` (an
`ids` target with `logical_delete`) and `get_receipt`, except that it refuses a
lookup of a `remember` receipt with 409 `memory_capability_unavailable` and
`details["feature"]` `remember_receipt_lookup` (the original `remember` response
carries the complete receipt); `find_facts`, `correct_facts`,
`get_erasure`, `resume_erasure` and options such as fact kinds, `dry_run`, `expire`,
`embedding_provider` and `as_of_ms` answer 409 `memory_capability_unavailable` with
`details["feature"]`. Body `deadline_ms` is the engine deadline, not the HTTP
`timeout`. Nothing is retried, split, paginated or embedded; after a
`TransportError` a mutation may have committed. Look up a `forget` or `consolidate`
with `get_receipt` and the same idempotency key. The server refuses a lookup of a
`remember` receipt, so repeat the same `remember` request (same key, same body) inside
the receipt window instead (not after its `retry_until_ms` without reading the state
first): an exact repeat returns the original result with
`replayed: true` and writes nothing. The client never repeats a request for you.

Licensed under MIT; see LICENSE.
