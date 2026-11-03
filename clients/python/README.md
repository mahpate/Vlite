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

Licensed under MIT; see LICENSE.
