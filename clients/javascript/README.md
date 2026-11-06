# VLite TypeScript / JavaScript client

ESM REST client for Node.js 22+ with TypeScript declarations and no runtime
dependencies. Install the local `clients/javascript` directory using npm.

See the [client guide](https://github.com/mahpate/Vlite/blob/main/clients/README.md)
for authentication, examples, supported operations and uncertain-write handling.
This package does not automatically retry requests or follow redirects.

## Key-value operations

The KV REST surface requires a server with KV routes and an existing,
dedicated one-dimensional `cache` collection. Namespaces are not authorization
boundaries. Run as an ESM script with `VLITE_URL` and `VLITE_AUTH_TOKEN` set:

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

`Expected` is `"any"`, `"absent"` or `{ revision: bigint }`. Values are
`Uint8Array`s (at most 1,047,290 bytes). Versions and deadlines are `bigint`,
never `Number`; use `expiresAtMs` for input and `expires_at_ms` on results.
Omitted/null expiry clears TTL; deadlines are absolute Unix milliseconds.
List uses exclusive `after` and returns `next_after`, without a cross-page
snapshot. Each operation accepts an optional abort `signal`. Cancellation or
timeout may hide a committed mutation; neither triggers a retry.
404 `kv_not_found` remains `VLiteError`; only 404 `not_found` becomes
`UnsupportedError`, without fallback. Writes sync only with server
`--durability full`.

## Memory operations

The agent-memory REST surface requires a server with a provisioned memory binding.
`client.memory(binding)` returns a binding-local facade whose nine methods each POST one request
object to `/v1/memory/{binding}/...` and resolve with the full response: `remember`, `recall`,
`findFacts`, `correctFacts`, `consolidate`, `forget`, `getReceipt`, `getErasure` and `resumeErasure`.
The first server slice supports remember (`entity` and `episodic` kinds), recall, consolidate
(retain dispositions, commit only), forget (`ids` target, logical delete) and receipt lookup, except
that it refuses a lookup of a `remember` receipt (the original `remember` response carries the
complete receipt). The other four methods, every request field beyond that slice and that lookup
answer 409 `memory_capability_unavailable` with `details.feature` (`remember_receipt_lookup` for the
lookup); the client never interprets, hides or works around a refusal.

```javascript
import { Client } from "@vlite/client";

const memory = new Client(process.env.VLITE_URL, process.env.VLITE_AUTH_TOKEN).memory("notes");
const scope = { agent: "assistant", user: "ada" };
const saved = await memory.remember({ scope, idempotency_key: "remember-1",
  items: [{ id: "ada", kind: "entity", content: "Ada", observed_at_ms: Date.now(),
    source: { source_id: "chat", source_kind: "caller" } }] });
console.log(saved.receipt.sequence, saved.outcomes);
console.log((await memory.recall({ scope, text: "Ada" })).memories);
```

Text and vector recall find only memories that hold an evidence record, so the recall above returns
nothing on a new binding, and a binding without an evidence collection refuses it with 409
`memory_capability_unavailable` (`details.feature` `evidence_recall`). Recall with
`entity_seeds: ["ada"]` to read back what you saved.

Requests use the wire (snake_case) names and are closed: an unknown member is a `TypeError` and is never
sent, and a required member (including `idempotency_key`) is never invented. u64 and i64 inputs accept a
canonical decimal string, a safe-integer `Number` or a `bigint`; an unsafe `Number` is a `TypeError`
instead of being rounded, and the client always sends decimal strings. u64 and i64 outputs stay decimal
strings. `deadline_ms` is the engine deadline, not the HTTP timeout (`timeoutMs`). Each method takes an
optional `{ signal }`. HTTP 202 resolves like 200: check `state`. Failures are `VLiteError` (`code`,
`status`, `requestId`, optional `details`), `UnsupportedError` for 404 `not_found` only, or
`TransportError` for a timeout, cancellation or lost connection, after which a mutation may have
committed. Look up a `forget` or `consolidate` with `getReceipt` and the same idempotency key. The
server refuses a lookup of a `remember` receipt (409 `remember_receipt_lookup`), so repeat the same
`remember` request, with the same key and body, inside the receipt window: an exact repeat returns the
original result with `replayed: true` and writes nothing. The client never repeats a request for you;
nothing is retried, embedded, split or paged.

Licensed under MIT; see LICENSE.
