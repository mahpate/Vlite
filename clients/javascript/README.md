# VLite TypeScript / JavaScript client

ESM REST client for Node.js 22+ with TypeScript declarations and no runtime
dependencies. Install the local `clients/javascript` directory using npm.

See the [client guide](https://github.com/mahpate/Vlite/blob/main/clients/README.md)
for authentication, examples, supported operations and uncertain-write handling.
This package does not automatically retry requests or follow redirects.
## Key-value operations

The unreleased KV REST surface requires a server with KV routes and an existing,
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

Licensed under MIT; see LICENSE.
