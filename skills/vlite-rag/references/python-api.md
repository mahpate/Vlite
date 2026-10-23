# Verified Python retrieval APIs

Examples match `bindings/python/vlite_embedded/__init__.py` and
`clients/python/vlite_client/__init__.py`. Use Python 3.11+. IDs/revisions are
stringified u64 values; payloads are base64 bytes. Match binding/native versions.

## Embedded: create, upsert, filter, fetch

```python
import base64
from vlite_embedded import NativeLibrary

native = NativeLibrary()  # wheel, or NativeLibrary(path_to_shared_library)
# Refuses overwrite. Format 4 enables named collections/settings.
with native.create("documents.vlite", 3, format=4) as db:
    db.create_collection("docs", 3, settings={
        "metric": "cosine",
        "metadata_indexes": [{"field": "tenant", "kind": "eq"}],
    })
    metadata = {
        "tenant": {"type": "string", "value": "demo"},
        "source": {"type": "string", "value": "handbook.md"},
        "chunk": {"type": "integer", "value": "0"},
    }
    text = "VLite stores vectors, metadata and payloads."
    # Toy vectors: replace BOTH document and query vectors with embeddings.
    db.put("1001", [1.0, 0.0, 0.0], collection="docs", metadata=metadata,
           payload_base64=base64.b64encode(text.encode("utf-8")).decode("ascii"))
    tenant_filter = [{"op": "eq", "field": "tenant",
                      "value": {"type": "string", "value": "demo"}}]
    hits = db.search([1.0, 0.0, 0.0], 5, collection="docs",
                     metric="cosine", filter=tenant_filter)
    for hit in hits:
        record = db.get(hit["id"], collection="docs")
        excerpt = base64.b64decode(record["payload_base64"]).decode("utf-8")
        print(record["metadata"]["source"]["value"], hit["id"], excerpt)
```

Reopen with `with native.open("documents.vlite", read_only=True) as db:`.
Close every reader before opening a writer; the two cannot coexist on one file.
`db.search(..., explain=True)` returns a dictionary with `hits`/`stats` instead
of a hit list. `db.search_auto(..., filter=tenant_filter, collection="docs",
explain=True)` returns a dictionary and selects exact or a ready ANN index
without building. Keep mandatory predicates in that call.

Explicit unfiltered ANN, within an open database context:

```python
db.build_index(collection="docs", metric="cosine")
hits = db.search_approximate_ready([1.0, 0.0, 0.0], 5,
                                  collection="docs", metric="cosine", ef_search=100)
```

Do not use this unfiltered wrapper for tenant-filtered retrieval.
Embedded `set_collection_settings(name, settings)` is a **full replacement**:
omitted fields revert to defaults. REST settings updates retain omitted fields.

## REST client: existing server, new collection

Install `vlite-client`, or add repository `clients/python` to `PYTHONPATH`.
Start an authenticated server first using its release's startup documentation.
This client does **not** create the server's database file. Named collections
require format 4+ and appropriate credential scopes. Read the existing token
from the environment; never hard-code it. The current client accepts tokens
containing 32–256 base64url characters.

```python
import base64
import os
from vlite_client import Client

client = Client(os.environ.get("VLITE_URL", "http://127.0.0.1:8080"),
                os.environ["VLITE_TOKEN"])
caps = client.capabilities()
print(client.database())
# Run ONCE where docs does not exist; inspect/reuse the collection on later runs.
client.create_collection("docs", 3, settings={
    "metric": "cosine",
    "metadata_indexes": [{"field": "tenant", "kind": "eq"}],
})
docs = client.collection("docs")
print(docs.info())
docs.put("1001", {
    "vector": [1.0, 0.0, 0.0],
    "payload_base64": base64.b64encode(b"VLite stores vector records.").decode("ascii"),
    "metadata": {
        "tenant": {"type": "string", "value": "demo"},
        "source": {"type": "string", "value": "handbook.md"},
    },
})
request = {
    "vector": [1.0, 0.0, 0.0], "k": 5, "mode": "exact", "metric": "cosine",
    "filter": [{"op": "eq", "field": "tenant",
                "value": {"type": "string", "value": "demo"}}],
}
for hit in docs.search(request):
    record = docs.get(hit["id"])
    print(hit["id"], base64.b64decode(record["payload_base64"]).decode("utf-8"))

# Explicit build costs CPU/RAM; schedule appropriately.
# Never remove mandatory filters to accommodate an older server.
if caps.get("features", {}).get("filtered_ann"):
    docs.build_index(metric="cosine")
    print(docs.index_status())
    approximate = dict(request, mode="approximate_ready", ef_search=100)
    print(docs.search(approximate))
```

`Client` is synchronous, has no context-manager or close method, never retries
mutations, and does not follow redirects. Reconcile an unknown mutation outcome
by ID before retrying. `search_detailed({... , "explain": True})` returns plans/
stats; `search(...)` returns just hits. Readiness must match collection, metric,
graph settings, quantization and rerank settings.

## Common typed values and filters

- String: `{"type": "string", "value": "demo"}`.
- Integer: `{"type": "integer", "value": "1700000000"}`.
- Float: `{"type": "float", "value": 0.75}` (finite).
- Boolean: `{"type": "boolean", "value": True}` in Python.
- Equality: `{"op": "eq", "field": "tenant", "value": TYPED_VALUE}`.
- Inclusive range: `{"op": "range", "field": "chunk", "min":
  {"type": "integer", "value": "0"}, "max": {"type": "integer", "value": "9"}}`.

Do not send untyped metadata or use `filters` (plural) in these Python APIs.
The offline shell has a different input shape; do not copy commands verbatim.
