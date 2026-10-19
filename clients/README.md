# VLite REST clients

First-party clients for Python 3.11+ and TypeScript/JavaScript (Node.js 22+).
They call the existing REST service; they are not embedded database bindings.
For server-free access, use the separate [Python/native bindings](../bindings/README.md)
or [embedded Node.js/TypeScript addon](../bindings/node/README.md).
Both use the MIT License. Install the wheel or tarball from
[GitHub release assets](../docs/INSTALL.md), or build from this checkout.
Packages are not published to PyPI or npm.

## Python

Install from the repository root:

```text
python -m pip install ./clients/python
```

Use the URL and token supplied by your deployment:

```python
import os
from vlite_client import Client, VLiteError

client = Client(os.environ["VLITE_URL"], os.environ["VLITE_AUTH_TOKEN"])
client.put("10", {
    "vector": [1, 0, 0],
    "payload_base64": "AP8=",
    "metadata": {"category": {"type": "string", "value": "example"}},
})
print(client.get("10"))
print(client.search({"vector": [1, 0, 0], "k": 5}))
page = client.list_vectors(limit=25)
while True:
    print(page["items"])
    if page["next_after"] is None:
        break
    page = client.list_vectors(limit=25, after=page["next_after"])
```

Python is synchronous and uses the standard library with a 30-second socket
timeout by default (`timeout=60` overrides it). Environment HTTP proxies are
disabled; use a directly reachable service or HTTPS reverse proxy.

## TypeScript / JavaScript

Install the local package from your application's directory, adjusting the path:

```text
npm install /path/to/VLite/clients/javascript
```

```javascript
import { Client, VLiteError } from "@vlite/client";

const client = new Client(process.env.VLITE_URL, process.env.VLITE_AUTH_TOKEN);
await client.put("10", {
  vector: [1, 0, 0],
  metadata: { category: { type: "string", value: "example" } },
});
console.log(await client.get("10"));
console.log(await client.search({ vector: [1, 0, 0], k: 5 }));
let page = await client.listVectors({ limit: 25 });
for (;;) {
  console.log(page.items);
  if (page.next_after === null) break;
  page = await client.listVectors({ after: page.next_after, limit: 25 });
}
```

The ESM package includes TypeScript declarations and uses native `fetch`;
there are no runtime dependencies. `timeoutMs` defaults to 30000 and bounds the
request including response consumption. See [the typed example](javascript/example.ts).
Browser use requires a same-origin deployment: VLite does not enable CORS.
Keep the full-access token out of shipped frontend code and public sites.
Node.js is the tested runtime; browser clients are not a separate supported SDK.

## Supported operations and wire contracts

Python uses `list_vectors`, `index_status`, `build_index`; JavaScript uses
`listVectors`, `indexStatus`, `buildIndex`. Both expose `health`, `ready`,
`database`, `metrics`, `get`, `put`, `delete`, `batch` and `search`.
`delete` returns a boolean; `search` returns the hit array. Mutations returning
HTTP 204 complete without a result. See [OpenAPI](../src/openapi.json) for
limits, metadata/filter shapes and server error codes.

- IDs are **canonical decimal strings**, including `18446744073709551615`.
  Integer metadata values are signed-i64 strings, not JavaScript numbers.
- Payloads remain canonical padded base64 strings; encode/decode binary data
  explicitly with your language's base64 utilities.
- Put replaces the entire record; omitted metadata/payload clears them.
  Batch accepts the REST operation objects and commits them atomically.
- Search accepts `exact`, `approximate` and `approximate_ready`. Filters require
  exact mode. Ready-only search never builds; explicitly call the build method.
- Listing returns IDs/metadata only, capped at 100 records and 1 MiB per page.
  Follow the returned cursor until null, even on short pages. Pages are live,
  not snapshots; index readiness is also only a point-in-time observation.
- Constructors require an HTTP(S) origin, without credentials, subpath, query
  or fragment, and a 32..256-character base64url bearer token. Use HTTPS beyond
  loopback. Tokens are private client state, never included in request URLs.
  Server-side validation remains authoritative; types do not replace validation.
- No automatic retries, redirects, index builds, repair, or fallback search.

## Errors and uncertain writes

`VLiteError` exposes HTTP `status`, server `code`, and `request_id` (Python) or
`requestId` (JavaScript). Missing records raise 404 rather than returning null.
Nonstandard HTTP errors use `http_error`; Python surfaces redirects as an HTTP
error, while fetch rejects them as `TransportError`.

`TransportError` means connection, timeout, response parsing or transport failure.
A write **may already have committed**, including after a timeout/disconnect.
Likewise, `commit_indeterminate` does not establish failure. Do not automatically
retry a mutation; inspect/reconcile current state and follow the recovery
contract. Clients do not expose credentials in their own error messages, but
application logs must still avoid logging tokens or sensitive returned data.

## Client development

Run from the repository root:

```text
python -m unittest discover -s clients/python -p test_client.py
npm --prefix clients/javascript ci
npm --prefix clients/javascript test
npm --prefix clients/javascript run typecheck
cargo build --locked --features server --bin vlite-server
python tests/clients_e2e.py target/debug/vlite-server
```

On Windows, pass `target\debug\vlite-server.exe` to the final command.
Integration tests start isolated servers on ephemeral loopback ports, create
temporary databases and tokens, exercise both languages and clean up.
