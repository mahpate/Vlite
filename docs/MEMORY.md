# Agent memory

Agent memory lets an AI agent keep what it learns between sessions. A caller remembers items in an exact scope, recalls them later with bounded, ranked retrieval, consolidates them into summaries and forgets them. It is built from a property graph and an optional evidence collection in the same database file, and it is off unless an operator provides a bindings file.

This guide covers VLite 0.7.1: what the server serves, how to use it from each interface and what it refuses. The reference text is in [Agent memory (REST)](API.md#agent-memory-rest), [Memory-bound resources](SECURITY.md#memory-bound-resources) and the [serving contract](projects/roadmap/serving-memory-contract.md). The shell examples are POSIX `sh` (bash, zsh, Git Bash or WSL): in PowerShell `$VLITE_URL` is empty, a backslash does not continue a line and `EXPIRES=$((...))` is not valid.

## Concepts

- **Binding.** A name that an operator provisions for one graph and at most one evidence collection, together with the scopes each credential may use and a receipt window. Callers use only the name, in `POST /v1/memory/{binding}/...`; they never name the graph or the collection. A binding name matches `^[a-z][a-z0-9_-]{0,62}$` and belongs to one tenant (`@root` or a tenant ID) that the caller never sends.
- **Scope.** `{"agent": "a"}`, optionally with a `user`, and with a `session` only when there is a `user`. Each part is 1 to 256 UTF-8 bytes without control characters. Matching is exact: a memory in `{"agent":"a","user":"u"}` is not visible in `{"agent":"a"}`, and the reverse. A mutation names one scope. Recall names one scope and may list `scopes` (at most 16 in total, `scope` included); the credential must be granted every one.
- **Memory item.** An `entity` (a thing, a person, a place) or an `episodic` memory (something that happened). It has an ID that you choose, text content of at most 4,096 bytes, an observation time, a source, an importance from 0 to 1 and an optional expiry. Facts and relations are not served.
- **Evidence.** Records in the binding's evidence collection that a memory holds. They carry the vectors and text that recall searches, and recall returns their snippets. Memory routes cannot create evidence records: a holder of `raw_access` writes them (see [Evidence references](#evidence-references-and-raw-access)).
- **Revision.** Each memory has a revision: 1 when it is created and one more for each merge. Forget and consolidate name the revision they expect.
- **Receipt and idempotency key.** Every mutation carries an `idempotency_key` in its body. Its receipt keeps the result for the binding's receipt window, so an exact retry returns the original result instead of writing twice (see [Receipts and retries](#receipts-and-retries)).

## What is served

| Route (all `POST`, under `/v1/memory/{binding}`) | Status in 0.7.1 |
|---|---|
| `/remember` | served: `entity` and `episodic` items |
| `/recall` | served: bounded, exact retrieval with `weighted`, `exact` and `mmr` selection |
| `/consolidate` | served: one caller-written summary of retained sources (commit only) |
| `/forget` | served: logical deletion by ID and revision |
| `/receipts/lookup` | served for `forget` and `consolidate`; a `remember` lookup answers 409 |
| `/facts/query`, `/facts/correct` | 409 `memory_capability_unavailable` |
| `/erasures/get`, `/erasures/resume` | 409 `memory_capability_unavailable` |

There are no `GET`, `HEAD` or `DELETE` aliases. A request must use `Content-Type: application/json`, be one JSON object with unique keys and be at most 8 MiB. Responses have no server size cap. See [What is unavailable](#what-is-unavailable).

## Set up a binding

An operator sets memory up once. The steps use the ordinary admin routes, so run them against a server that is already in protected mode with an admin token and an admin directory (see [server setup](REFERENCE.md#rest-service-optional)); scoped credentials need both. Start one first, without the bindings file, and use the same `VLITE_ADMIN_DIR` for every later start, because the credentials live in the admin directory. This runs the server in the background of one shell; run the steps in that shell:

```sh
export VLITE_URL=http://127.0.0.1:8080 VLITE_ADMIN_DIR=./admin
python3 -c "import secrets; print(secrets.token_urlsafe(32))" > admin.token
export VLITE_ADMIN_TOKEN=$(cat admin.token)
vlite-server data.vlite --bind 127.0.0.1:8080 --create-if-missing 3 --create-format 5 --token-file admin.token & SERVER_PID=$!
```

1. **Create the graph and the evidence collection.** Memory needs a database with graph support (every new file has it). The evidence collection must be a new collection, not `default`. Its dimension is the length of the vectors you will store, and `text_fields` names the metadata fields that text recall searches.

   ```sh
   curl -sS -X POST "$VLITE_URL/v1/graphs" \
     -H "Authorization: Bearer $VLITE_ADMIN_TOKEN" -H "Content-Type: application/json" \
     -d '{"graph":"notes_graph"}'

   curl -sS -X POST "$VLITE_URL/v1/collections" \
     -H "Authorization: Bearer $VLITE_ADMIN_TOKEN" -H "Content-Type: application/json" \
     -d '{"name":"notes_ev","dimensions":3,"settings":{"text_fields":["text"]}}'
   ```

2. **Create two scoped credentials: one for the agent and one for the raw holder that the evidence example needs.** Run this step twice, with the names `notes-agent` and `notes-raw`: run the command as it is for `notes-agent`, then set `NAME=notes-raw` and run it again. A memory credential needs `graph_read` and `graph_write` for the graph, and `read`, `search` and `write` on the evidence collection. Name the collection or use `["*"]`. Once a bindings file reserves the collection, a request that names it answers 404, so create the credential first or give it `["*"]`. The response carries the `token`, which is shown once: set `VLITE_TOKEN` to the token of `notes-agent` and `VLITE_RAW_TOKEN` to the token of `notes-raw`. Each `credential.id` (12 characters) is a grant's principal in step 3. The client examples below read `VLITE_AUTH_TOKEN`: give it the same token as `VLITE_TOKEN`, never the admin token.

   ```sh
   EXPIRES=$(( ($(date +%s) + 30 * 86400) * 1000 ))
   curl -sS -X POST "$VLITE_URL/v1/admin/credentials" \
     -H "Authorization: Bearer $VLITE_ADMIN_TOKEN" -H "Content-Type: application/json" \
     -d "{\"name\":\"${NAME:-notes-agent}\",\"collections\":[\"*\"],\"graphs\":[\"notes_graph\"],\"operations\":[\"graph_read\",\"graph_write\",\"read\",\"search\",\"write\"],\"expires_unix_ms\":$EXPIRES}"
   ```

   Each route asks for less than that, but every route requires the credential to name the evidence collection (or hold `["*"]`) and the graph:

   | Route | Graph operation | Evidence collection operations |
   |---|---|---|
   | `recall` | `graph_read` | `read`, `search` |
   | `receipts/lookup` | `graph_read` | none |
   | `remember`, `forget` | `graph_write` | `write` |
   | `consolidate` | `graph_write` | none |

3. **Write the bindings file.** Replace `abcdefghij27` with the ID of the `notes-agent` credential and `klmnopqrst27` with the ID of `notes-raw`. The file is strict JSON of at most 1 MiB; keep it in a directory that only the operator can write.

   ```json
   {"version": "memory-bindings-v1", "bindings": [{
     "binding": "notes", "tenant": "@root", "generation": 1, "state": "active",
     "graph": "notes_graph", "evidence_collection": "notes_ev",
     "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
     "digest_version": "memory-request-v1", "receipt_window_ms": 86400000,
     "grants": [{"principal": "abcdefghij27", "scopes": [{"agent": "a"}, {"agent": "agent-1"}, {"agent": "assistant", "user": "ada"}]},
                {"principal": "klmnopqrst27", "scopes": [{"agent": "a"}], "raw_access": true}]
   }]}
   ```

   A grant lists the exact scopes that credential may use; the scopes above are the ones the examples in this guide use. `"raw_access": true` on a grant lets that credential also reach the graph and the collection through the raw routes: the `notes-raw` credential needs it for the evidence example. `receipt_window_ms` runs from 1 to 2,592,000,000 (30 days). A file holds at most 256 bindings, 1,024 grants per binding and 256 scopes per grant. After the first start with the file, a binding's receipt window, graph and evidence names never change. Raise `generation` when grants or state change and keep it otherwise; startup refuses both mistakes. Editing the file after the first start, for example to add a grant, without a higher `generation` makes startup exit with `a memory binding changed without a higher generation`. [Start the server with memory](API.md#start-the-server-with-memory) lists every rule, including retiring a binding.

4. **Stop that server (`kill $SERVER_PID`) and restart it with the file.**

   ```sh
   vlite-server data.vlite --bind 127.0.0.1:8080 --create-if-missing 3 --create-format 5 --token-file admin.token --memory-bindings-file bindings.json & SERVER_PID=$!
   ```

   `VLITE_MEMORY_BINDINGS_FILE` does the same. Restart with the same `VLITE_ADMIN_DIR` and token file as before: the credentials live in the admin directory, so without it the credential from step 2 no longer exists. Startup refuses to run when the graph or the collection is missing, when a resource already holds ordinary data, or when the file breaks a rule, and its message never names the binding. While a binding is configured, the server refuses `--grpc-bind` and automatic index maintenance, and imports, replication export and some aggregate admin routes answer 403. Without the file there are no memory routes, but a graph-enabled database still has its binding records read at startup: the server refuses to start if they cannot be read or are not valid, and if binding records remain after the file is removed, `--grpc-bind` and automatic index maintenance stop the start.


Neither the admin token nor the read-only token can use the memory routes: a binding grants a credential, and only scoped credentials are granted.

## Remember

```sh
curl -sS -X POST "$VLITE_URL/v1/memory/notes/remember" \
  -H "Authorization: Bearer $VLITE_TOKEN" -H "Content-Type: application/json" \
  -d '{"scope":{"agent":"a"},"idempotency_key":"k1","items":[{"id":"e1","kind":"entity","content":"Ada","observed_at_ms":"90","source":{"source_id":"s1","source_kind":"caller"}}]}'
```

The request is the first exchange of the REST wire fixtures. Use a real observation time in your own requests. The answer is a receipt and one outcome per item, in input order:

```json
{"receipt":{"id":"r1","operation":"remember","scope":{"tenant":"@root","agent":"a"},"sequence":"7","retry_until_ms":"200","digest_version":"memory-request-v1"},"replayed":false,"outcomes":[{"id":"e1","action":"created","revision":"1"}],"feed":{"mode":"ordinary_changes","temporal_complete":false,"ttl_complete":false,"consumer_purge_acknowledged":false},"remaining_work":"0"}
```

The receipt `id` is an opaque string. The `sequence` is the commit sequence of the write, and `retry_until_ms` is the end of the receipt window.

An item has these fields:

| Field | Meaning |
|---|---|
| `id` | Required. Your ID for the item, 1 to 256 bytes, unique in the request. |
| `kind` | Required: `entity` or `episodic`. |
| `content` | Required. UTF-8 text of at most 4,096 bytes. |
| `observed_at_ms` | Required. When it was observed, in Unix milliseconds, as an integer or a decimal string. A time later than the server's clock is refused with 422, so a client whose clock runs ahead needs a margin. |
| `source` | Required: `source_id` and `source_kind`, and optionally `uri` and `transformation_id`, each 1 to 256 bytes. |
| `importance` | Optional, 0 to 1 (default 0.5). |
| `expires_at_ms` | Optional absolute expiry of the memory, in Unix milliseconds. Expiry is a logical deletion and does not delete the memory's evidence record (see [Forget](#forget)). |
| `provenance` | Optional list of up to 64 sources (default: just `source`). |
| `references` | Optional `{id, revision}` entries that name other items of the same request, with revision `0`. They become the links that recall follows from an entity seed. At most `max_references` (1 to 256, default 64). |
| `merge`, `expected` | `"merge": "merge_provenance"` with `"expected": {"revision": N}` adds a source to an existing memory (see below). |
| `evidence` | Optional, one reference to an existing evidence record (see [Evidence references](#evidence-references-and-raw-access)). |

The request also takes `deadline_ms` (1 to 5,000, default 250), the time budget for the call.

Remember never overwrites a memory. A request is one commit, so it succeeds or fails as a whole, and no item is reported separately.

- An item that repeats an existing memory (same scope, kind, content and a source that memory already lists) changes nothing: its outcome is `noop` with the memory's current revision. The match is by scope, kind, content and source, not by id: a repeat under another id answers `noop` with the `id` and `revision` of the memory that exists.
- An item whose `id` already exists with other content, kind or source is refused with 412 `precondition_failed`.
- To add another source to an existing memory, send an item with the same `id`, kind and content, `"merge": "merge_provenance"` and `"expected": {"revision": N}` for its current revision. The outcome is `merged` with revision N + 1. A wrong revision answers 412, an unknown memory 404 `memory_not_found`.
- Two items with the same `id` in one request answer 422 `invalid_input`.
- A request commits as one batch of at most 4,096 graph operations and 16 MiB. Each item takes several operations, so the largest request holds fewer than 4,096 items. A request over either bound answers 422.

## Recall

Recall is bounded: you state how many results you want and how much work it may do. It reads the retained state at the server's clock and does not paginate.

### What recall searches

- **Entity seeds.** `entity_seeds` lists IDs of `entity` memories in the scope. Recall returns them and the memories that they reference, following references outward for at most `limits.max_hops` hops (default 2, at most 4). A seed that does not exist answers 404 `memory_not_found`; a seed that is not an entity answers 422.
- **A vector.** The vector is compared with the vectors of the evidence records that memories hold. It is an exact scan: no approximate index is used or built. The answer is the memory that holds the record.
- **Text.** `text` is matched lexically (BM25) against the metadata fields that the evidence collection declares in `text_fields`. The server does not turn text into a vector. The evidence collection must declare `text_fields` and its text index must be ready (409 `index_not_ready` otherwise). Text of 1 to 1,024 bytes is served; longer text up to 16,384 bytes answers 409 with `details.feature` `query_text_length`.

A request needs at least one of `vector`, `text` and `entity_seeds`. A binding without an evidence collection cannot answer a vector or text query: 409 `memory_capability_unavailable` with `details.feature` `evidence_recall`. So a memory can be found by a vector or by text only when it holds an evidence record.

### The request

| Field | Meaning |
|---|---|
| `scope` | Required. `scopes` optionally lists at most 16 scopes in total, `scope` included. |
| `entity_seeds`, `vector`, `text` | The query, as above. `vector` has at least one finite number. |
| `kinds` | Up to 4 distinct kinds. Default: `entity` and `episodic`. |
| `limit` | 1 to 100 results (default 20). It cannot exceed `limits.candidates`. |
| `ranking` | `selection` (`weighted`, `exact` or `mmr`), `weights`, `half_life_ms`, `future_observations`, `lambda`. |
| `limits` | The work budgets (see below). |
| `projection` | `snippets` and `provenance`, both true by default. |
| `mode` | `exact` (the default). `approximate` answers 422. |
| `allow_partial` | Does not produce partial results: see below. |

The work budgets and their defaults are: `candidates` 1 to 1,000 (200), `vector_components` up to 4,000,000 (1,000,000), `evidence_bytes` up to 4,194,304 (1,048,576), `mmr_components` up to 100,000,000 (10,000,000), `deadline_ms` 1 to 5,000 (250), and for the graph walk `max_hops` 0 to 4 (2), `max_visited_nodes` up to 10,000 (1,000), `max_edges_examined` up to 100,000 (10,000), `max_edges_per_node` up to 10,000 (1,000), `max_output_nodes` up to 1,000 (100), `max_output_edges` up to 2,000 (200) and `max_output_paths` up to 100 (20). A request that would exceed a budget answers 422 `invalid_input` with `details.field` naming it, and `allow_partial: true` turns that into 422 `memory_bounded_plan_unsupported`, because a partial answer is never served. Exceeding `deadline_ms` answers 504 `deadline_exceeded`. Recall also refuses a source of more than 100,000 records (the memories and the evidence records it would have to scan) with 422.

### Selections

- **`weighted`** is the default. It ranks candidates by `S = wR·R + wD·D + wI·I`: `R` is `1 / rank` of the candidate in the fused list of the vector and text legs (0 for a memory reached only by expansion), `D` is a decay by age with half life `half_life_ms`, and `I` is the item's importance. The default weights `[1, 0, 0]` use relevance only. `weights` are three finite, nonnegative numbers that sum to 1; a decay weight above 0 needs a positive `half_life_ms`; `future_observations` is `reject` (default) or `clamp`. The vector leg measures **squared Euclidean distance**.
- **`exact`** needs a vector, may add `text`, and orders the vector candidates by **cosine distance**. It takes no seeds, no other weights and no half life.
- **`mmr`** needs a vector and nothing else. It selects by cosine distance with a diversity trade-off, `lambda` from 0 to 1 (default 0.5), and returns its selection order. `S` is then a diagnostic value, not the objective.

A request that `exact` or `mmr` cannot serve (no vector, seeds, other weights, a half life, or text with `mmr`) answers 422 `memory_unsupported_ranking`.

### Distance metrics

| Selection | `scores.distance` | Range |
|---|---|---|
| `weighted` | squared Euclidean distance of the query | 0 for identical vectors, unbounded |
| `exact`, `mmr` | cosine distance `1 - cos` | 0 same direction, 1 orthogonal, 2 opposite |

A query vector of zero length has no direction, so `exact` and `mmr` answer 422 `invalid_input`; a stored zero vector is never a vector candidate of `exact` or `mmr`. In 0.7.0 all three selections measured squared Euclidean distance. For unit-length vectors the order is unchanged and the new distance is half the old one; for other vectors the order of `exact` hits can change, so recalibrate any threshold on `scores.distance`.

The metric matters when vectors are not unit length. Take the query `[2, 0, 0]` and four evidence records:

| Record | Vector | `weighted` distance | `exact` distance |
|---|---|---|---|
| `ev-a` | `[10, 0, 0]` | 64 | 0 |
| `ev-b` | `[0.6, 0.6, 0]` | 2.32 | 0.2929 (`1 - 0.6 / sqrt(0.72)`) |
| `ev-c` | `[0, 1, 0]` | 5 | 1 |
| `ev-z` | `[0, 0, 0]` | 4 | not a candidate (no direction) |

`weighted` ranks `ev-b`, `ev-z`, `ev-c`, `ev-a`, the nearest in space first. `exact` ranks `ev-a`, `ev-b`, `ev-c`, because `ev-a` points the same way as the query.

### Exact with text

`exact` accepts a `text` next to its vector. The vector candidates are ordered by cosine distance, the text candidates by BM25, and the two lists are fused by reciprocal rank (equal weights, `rrf_k` 60). A candidate that only the text leg found is kept, with its `text_rank` and `bm25` and no `scores.distance`. `mmr` with `text` still answers 422 `memory_unsupported_ranking`. With the four records above, where the records hold the text "lore lore lore", "other other other", "lore lore other" and "lore other other" in their `text` field, the query `{"vector": [2, 0, 0], "text": "lore", "ranking": {"selection": "exact"}}` returns `ev-a`, `ev-c`, `ev-b`, `ev-z`: `ev-z` comes from the text leg alone.

### Reading a result

```json
{"memories":[{"id":"a","revision":"1","scope":{"tenant":"@root","agent":"a"},"kind":"entity","content":"a","observed_at_ms":"90","created_at_ms":"100","updated_at_ms":"100","importance":0.5,"status":"accepted","stale":false,"segment":null,"evidence":[{"collection":"notes","record":"1168962963564408392","snippet":"a"}],"scores":{"R":1.0,"D":null,"I":0.5,"S":1.0,"distance":0.0,"dense_rank":1,"fused_rank":1},"provenance":[{"source_id":"s1","source_kind":"caller","content_digest":"v1:ac8d…"}],"references":[],"metadata":{}}],"meta":{"scopes":[{"tenant":"@root","agent":"a"}],"as_of_ms":"100","ttl_clock_ms":"100","sequence":"6","complete":true,"truncation":[],"limitations":["retained_state_only"],"mode":"exact_enumeration","candidates_examined":"2","edges_charged":"0","vector_components":"4","mmr_components":"0","evidence_bytes":"2"},"ranking_version":"memory-ranking-v1","selection":"exact"}
```

This is one memory of the `recall-vector-exact` wire fixture, shortened. A memory carries its `revision` (use it for forget, consolidate and merge), its `evidence` (the binding's name as `collection`, the record number and a snippet of at most 4,096 bytes) and its `scores`: `R`, `D`, `I` and `S`, plus `distance`, `dense_rank`, `text_rank`, `bm25` and `fused_rank` where they apply. `meta` reports the scopes, the clocks, the commit `sequence`, the work that was charged and `complete`. Every successful read lists `limitations: ["retained_state_only"]`: recall sees what is retained now, not history. Numbers that can exceed 53 bits (times, revisions, record numbers, counts) are decimal strings.

## Evidence references and raw access

Vector and text recall find a memory through the evidence record that it holds, and the memory routes cannot create records. The flow is therefore:

1. A credential with `raw_access` on the binding writes a record into the evidence collection through the ordinary record route. For text recall, put the text in a field that the collection declares in `text_fields`; the payload becomes the snippet.

   ```sh
   curl -sS -X PUT "$VLITE_URL/v1/collections/notes_ev/vectors/7001" \
     -H "Authorization: Bearer $VLITE_RAW_TOKEN" -H "Content-Type: application/json" \
     -d '{"vector":[10,0,0],"payload_base64":"bG9yZSBsb3JlIGxvcmU=","metadata":{"text":{"type":"string","value":"lore lore lore"}}}'
   ```

2. The same credential remembers an item that references the record. Give the item content that no other memory of the scope has: a repeat is matched to the memory that exists (see [Remember](#remember)), and a record that memory does not hold is refused. `collection` is the binding's name, and `record` is a `u64` as an integer or a decimal string:

   ```json
   {"scope":{"agent":"a"},"idempotency_key":"k2","items":[{"id":"ev-a","kind":"entity","content":"Ada's notebook","observed_at_ms":"90","source":{"source_id":"s1","source_kind":"caller"},"evidence":[{"collection":"notes","record":7001}]}]}
   ```

The rules:

- **At most one reference per item.** A longer list, an unknown field or a `record` that is not a `u64` answers 422 `invalid_input` with `details.field` `evidence`. An empty list is the same as none.
- **It needs `raw_access`.** Attaching claims a record that the memory routes cannot otherwise reach. Without `raw_access` on the grant, or with a `collection` that is not the binding's name, the answer is the same 403 as for an unknown binding, before any record is looked at.
- **One conflict for every failure to attach.** A record that is missing, hidden or already held by a memory answers 409 `memory_attachment_conflict`, so the answer does not say which.
- **Attaching transfers the record to the memory.** Recall returns it, and `forget` deletes it with the memory unless another retained memory uses it. Attach only a record you do not need on its own.
- **The request-level `evidence` list stays refused** (409 with `details.feature` `evidence`), and so does an item's list on a binding without an evidence collection.
- **Replays.** An exact retry replays. A retry that names another record answers 409 `idempotency_conflict`. An item that repeats an existing memory must name the evidence that memory already holds, or it answers 409 `memory_attachment_conflict`. To attach a record to a memory that exists, merge a new source into it (`merge_provenance` with the expected revision); that attaches the record only when the memory holds none.
- **It is a one-way change for the file.** A memory that holds a reference is stored with a token that a 0.7.0 server cannot read, so do not roll back to 0.7.0 after the first reference. To recover, run 0.7.1 or later and `forget` the memory. See the [release notes](RELEASE-0.7.1.md#rollback).

## Consolidate

Consolidate records a summary that you wrote. The server does not summarize anything: you send the text, and it stores it as an `episodic` memory with a link to the memories it came from. The sources are kept.

```json
{"scope":{"agent":"a"},"idempotency_key":"c1","inputs":[{"id":"e1","revision":"1"}],"output":{"id":"summary1","kind":"episodic","content":"Caller summary","observed_at_ms":"90","source":{"source_id":"summary-source","source_kind":"caller","transformation_id":"summary-v1"}}}
```

- `inputs` lists 1 to 64 distinct memories of the scope, each with the revision you saw. A different current revision answers 412; a missing memory 404; a repeated input 422.
- `output` is an `episodic` item whose `source` names a `transformation_id`, the identity of whatever produced the text. Without it the answer is 422.
- The answer has one outcome: `created`, revision 1.
- A `dry_run` (feature `consolidate_plan`), a `logical_delete` disposition (`consolidate_delete`), a request-level `evidence`, and in the output a `confidence`, a non-empty `references`, an `evidence`, `metadata`, `assertion` or `expected`, or a `merge` other than `none` are refused with 409 `memory_capability_unavailable`.

## Forget

Forget deletes memories by ID. It is a logical deletion: the memory and its content are removed from the live graph, and the evidence record that only this memory used is deleted with it.

```json
{"scope":{"agent":"a"},"idempotency_key":"f1","target":{"ids":[{"id":"e1","revision":"1"}]},"mode":"logical_delete"}
```

- `target.ids` lists 1 to 4,096 memories, each with its current revision (1 or more). A stale revision answers 412 and a missing memory 404; nothing is deleted unless every target matches.
- The answer lists a `deleted` outcome per memory, `"state": "logical"` and an `erasure_notice`: `live_state_only: true`, `secure_erasure: false`, `external_copies: "operator_managed"`, `restore_requires_manifest: true` and `backup_history_purge: "not_verified"`. It has no `manifest_id`.
- **Forget is not secure erasure.** Backups, replicas, exported change logs and other copies keep what they held. Manage them separately.
- **Expiry is logical deletion too.** A memory whose `expires_at_ms` has passed is hidden from reads and later removed from the graph, but its evidence record is not deleted with it: the memory's expiry does not carry over to the record, which stays in the evidence collection until someone with `raw_access` deletes it, or until it expires on its own if it was given an expiry. Only `forget` deletes the evidence record that only the forgotten memory used. Neither expiry nor forget is secure erasure.
- Targets by series or entity (409, feature `forget_target`), by session or scope or with `allow_job: true` (`erasure_jobs`), and `mode: expire` (`forget_expire`) are refused with 409.

## Receipts and retries

Every mutation answers with a receipt, and a mutation is safe to repeat exactly:

- **A replay.** The same `idempotency_key` and the same request, inside the receipt window, returns the original result with `"replayed": true` and writes nothing. The same key with a different request answers 409 `idempotency_conflict`. After `retry_until_ms` the key may run again; expiry ends the replay guarantee, not the memory.
- **The key's scope.** A key belongs to the binding and the memory scope, not to the credential. Credentials that may write the same scope are peers: one can replay another's request, and read its receipt, by repeating the key.
- **Lookup.** `POST /v1/memory/{binding}/receipts/lookup` takes `{"scope": …, "operation": "forget" | "consolidate", "idempotency_key": …}` and returns `{"result": …}` with `replayed: true`. An unknown or expired receipt answers 404 `receipt_not_found`.
- **No lookup for `remember`.** A `remember` lookup answers 409 `memory_capability_unavailable` with `details.feature` `remember_receipt_lookup`. The original `remember` answer carries the complete receipt, so keep it.
- **After an uncertain write.** A timeout, a lost connection or a 503 `commit_indeterminate` does not prove that the write failed. Look up the receipt of a `forget` or `consolidate`; for a `remember`, repeat the same request with the same key and body inside the window. Do not change the body, and do not repeat it after `retry_until_ms` without reading the state first.

## Use it from your language

These examples are the ones that ship with the repository. Each assumes a server with a provisioned binding named `notes` and a credential that is granted the scope it uses.

### REST

The `curl` examples above are the REST examples. The wire fixtures in `tests/fixtures/agent_memory/wire/` hold more requests and answers for every route. The dashboard's MemoryAI workspace uses the same routes (see the [dashboard guide](CONSOLE.md#memoryai)).

### Python

From the [Python client guide](../clients/python/README.md#agent-memory):

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

Text and vector recall find only memories that hold an evidence record, so the text recall above returns nothing on a new binding. Recall with `"entity_seeds": ["e1"]` to see what you saved.

### JavaScript

From the [JavaScript client guide](../clients/javascript/README.md#memory-operations):

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

Text and vector recall find only memories that hold an evidence record, so this recall returns nothing on a new binding. Recall with `entity_seeds: ["ada"]` to see what you saved.

### .NET

From the [.NET client guide](../clients/README.md#net-agent-memory-operations), in a .NET 8 console application that references `VLite.Client`:

```csharp
using VLite;

using var client = new Client(Environment.GetEnvironmentVariable("VLITE_URL")!,
                              Environment.GetEnvironmentVariable("VLITE_AUTH_TOKEN")!);
long observedAtMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
var memory = client.Memory("notes");
var scope = new MemoryScope("agent-1");
var saved = await memory.RememberAsync(new MemoryRememberRequest(scope, "key-1",
    [new MemoryItem("e1", MemoryKind.Entity, "Ada", observedAtMs, new MemorySource("s1", "caller"))]));
var found = await memory.RecallAsync(new MemoryRecallRequest(scope, Text: "Ada", Limit: 5));
Console.WriteLine(saved.Receipt.Sequence);
```


Text and vector recall find only memories that hold an evidence record, so this recall returns nothing on a new binding. Recall with `EntitySeeds: ["e1"]` to see what you saved.

All three clients have nine methods, one `POST` each, and return the full response. They never retry, split, page, embed or invent an idempotency key, and they surface a refusal with its `details.feature` unchanged. A timeout or lost connection throws a transport error: the mutation may have committed.

### MCP

Build `vlite-mcp` with `mcp-proxy` (the release archives include it) and point it at the server. The memory tools exist only with `--server`. Read-only is the default; the example below uses `--writable`, because it calls a write tool:

```sh
vlite-mcp --server "$VLITE_URL" --token-env VLITE_TOKEN --writable
```

Without `--writable` the server lists `vlite_memory_recall`, `vlite_memory_facts`, `vlite_memory_receipt` and `vlite_memory_erasure`; with it, it also lists `vlite_memory_remember`, `vlite_memory_correct_facts`, `vlite_memory_consolidate`, `vlite_memory_forget` and `vlite_memory_resume_erasure`. Each tool takes flat arguments plus a required `binding`:

```json
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"vlite_memory_remember","arguments":{"binding":"notes","scope":{"agent":"a"},"idempotency_key":"k1","items":[{"id":"e1","kind":"entity","content":"Ada","observed_at_ms":"90","source":{"source_id":"s1","source_kind":"caller"}}]}}}
```

The token comes from the environment variable that `--token-env` names. Memory calls use HTTPS except for loopback hosts, ignore ambient proxies and time out after five seconds. See [Server-backed agent memory](MCP.md#server-backed-agent-memory).

### Command line

`vlite-memory-cli` sends one request file to a binding with one verb: `remember`, `recall`, `facts`, `correct-facts`, `consolidate`, `forget`, `receipt`, `erasure` or `resume-erasure`. It is built with the `mcp-proxy` feature and is in the release archives, not in the Docker images.

```sh
vlite-memory-cli --url "$VLITE_URL" --binding notes --token-env VLITE_TOKEN remember --request remember.json
vlite-memory-cli --url "$VLITE_URL" --binding notes --token-env VLITE_TOKEN recall --request recall.json
```

Save the two bodies below as `remember.json` and `recall.json`. They are the CLI test fixtures:

```json
{"scope":{"agent":"a"},"idempotency_key":"k1","items":[{"id":"e1","kind":"entity","content":"Ada","observed_at_ms":"90","source":{"source_id":"s1","source_kind":"caller"}}]}
```

```json
{"scope":{"agent":"a"},"entity_seeds":["e1"],"limit":1}
```

A success prints one JSON line. A failure prints one coded line, `{"error":{"code":"…","message":"…"}}`, on stderr, with exit code 2 for usage and input errors, 3 for authentication, 4 for a conflict or precondition, 5 for a deadline and 6 for transport ambiguity. See [Remote agent memory](TOOLS.md#remote-agent-memory).

## Limits and error codes

| Item | Limit |
|---|---|
| Request body | 8 MiB, `application/json`, unique keys |
| Items per `remember` | 1 to 4,096, within 4,096 graph operations and 16 MiB per commit |
| Item `content` | 4,096 bytes |
| IDs, keys, scope parts, source fields | 1 to 256 bytes, no control characters |
| Provenance entries per item | 64 |
| `max_references` | 1 to 256 (default 64) |
| `deadline_ms` | 1 to 5,000 (default 250) |
| Recall `limit` / `limits.candidates` | 1 to 100 (20) / 1 to 1,000 (200) |
| Recall scopes, seeds, kinds | 16, 64, 4 |
| Recall `text` | 1,024 bytes served, 16,384 bytes accepted |
| Consolidate inputs | 64 |
| Forget targets | 4,096 |
| Recall source scan | 100,000 records |

A refusal is `{"error":{"code":"…","message":"…","details":{…}}}`. `details.field` names an invalid field, and `details.feature` names a capability that is not served.

| Status | Code | Meaning here |
|---|---|---|
| 400, 413, 415 | `invalid_json` | Not JSON, duplicate keys, too large, or the wrong media type. A JSON value that is not an object answers 422 `invalid_input` (`details.field` `body`). |
| 401 | `unauthorized` | No or an invalid credential. |
| 403 | `forbidden_scope` | No grant for the binding, the scope, the graph or the evidence collection, or a credential that is not scoped. An unknown binding gives the same answer. |
| 403 | `read_only`, `tenant_suspended`, `tenant_unavailable` | The node is read-only, or the tenant is not available. |
| 404 | `memory_not_found`, `receipt_not_found` | A named memory, seed or receipt does not exist. |
| 409 | `memory_not_configured` | The scope is not provisioned. |
| 409 | `memory_capability_unavailable` | Not served in this release; `details.feature` says what (it also takes the values `binding_retired` and `binding_generation`). |
| 409 | `memory_feed_unsupported` | `require_complete_feed: true`. |
| 409 | `idempotency_conflict` | The key was used with a different request. |
| 409 | `memory_attachment_conflict` | An evidence record cannot be attached. |
| 409 | `index_not_ready` | The text index of the evidence collection is not ready. |
| 412 | `precondition_failed` | A revision or identity does not match. |
| 422 | `invalid_input` | A wrong type or shape, an unknown or missing field, an out-of-range value or an exceeded budget. |
| 422 | `memory_unsupported_ranking` | The selection cannot serve this request. |
| 422 | `memory_bounded_plan_unsupported` | `mode: approximate`, or `allow_partial: true` with an exhausted budget. |
| 429 | `rate_limited`, `quota_exceeded` | Credential limits; `Retry-After: 1`. |
| 503 | `busy`, `unavailable`, `poisoned`, `commit_indeterminate` | See [Receipts and retries](#receipts-and-retries). |
| 504 | `deadline_exceeded` | The time budget ran out before the commit point. |
| 500 | `internal_error` | A sanitized unexpected failure. |

The OpenAPI document also names codes for capabilities that are not served (`memory_slot_conflict`, `memory_deletion_pending`, `memory_job_reconciliation_required`, `erasure_not_found`); this release does not produce them.

## What is unavailable

These are refused, never ignored or partly applied: 409 `memory_capability_unavailable` with `details.feature`, after authorization and before the rest of validation: a request whose `scope` is missing, invalid or not granted is refused first (422 or 403), and so is a write to a read-only node (403 `read_only`).

| Route | Refused (`details.feature`) |
|---|---|
| `facts/query`, `facts/correct` | `facts` |
| `erasures/get`, `erasures/resume` | `erasure_jobs` |
| `remember` | `fact_kinds` (fact and relation items), `confidence`, `metadata`, `assertion`, `evidence` (the request's own list, or an item's list on a binding without an evidence collection), `reference_revision_pin`, `expected` (unless `merge` is `merge_provenance`) |
| `recall` | `embedding_provider`, `fact_kinds`, `filter`, `temporal_read` (`as_of_ms`), `paths`, `metadata` (projections), `evidence_recall`, `query_text_length` |
| `consolidate` | `consolidate_plan` (`dry_run`), `consolidate_delete`, `fact_kinds`, `confidence`, `references`, `evidence`, `metadata`, `assertion`, `expected`, `merge` |
| `forget` | `forget_target` (series or entity), `erasure_jobs` (session or scope target, `allow_job`), `forget_expire` (`mode: expire`) |
| `receipts/lookup` | `remember_receipt_lookup` |

Approximate recall answers 422 `memory_bounded_plan_unsupported`: no approximate index is built for a binding, and none is built on demand. Memory has no provider that turns text into vectors; send the vector yourself.

## Security model

- **Only scoped credentials.** A binding grants credential IDs exact scopes. The admin token and the read-only token hold no grant and are refused (403), and so is a credential without a grant for the binding or scope: the answer is the same 403 as for an unknown binding, so a caller cannot probe which bindings exist. There is no parent, child or sibling fallback between scopes.
- **Memory grants are not raw access.** A credential also needs `graph_read` or `graph_write` for the graph and the evidence permissions its route uses. Memory grants never confer access to the graph or the collection through the raw routes.
- **Reserved resources look missing.** For every caller that does not hold `raw_access` on the binding (the administrator token included), the binding's graph and evidence collection answer as if they did not exist on every REST and webhook path: lists omit them, a read answers 404 or 409 as for a missing name and a create answers as for an occupied name. The operator can see such attempts in the audit log as `reserved_hidden_attempt`, without the name. `raw_access` is granted only in the bindings file, and the holder still needs the ordinary tenant, collection and operation permissions.
- **Raw writers cannot forge memory.** `POST /v1/graphs/{graph}/batch` refuses nodes with memory's reserved keys and labels with 422, on every graph.
- **What a binding changes elsewhere.** While any resource is reserved, the backup routes are the administrator's alone (scoped credentials get 403), global metrics, graph statistics, replication export, webhook list and status, imports and the automatic-maintenance routes answer 403 `forbidden_scope` to credentials, and the gRPC listener does not start.
  The dashboard shows those routes as unavailable, and sign-in keeps working, even for the super admin ([details](CONSOLE.md#monitor-the-server)).
- **Audit.** Each memory request writes a `memory_request` event with the operation, binding, principal, `actor`, outcome and, for a mutation, the receipt ID and sequence. Memory text, keys and identifiers are never logged. See [Audit actors](SECURITY.md#audit-actors).
- **Known limits.** The commit sequence is database-wide, so `sequence` values, graph revisions and the change-feed head show that other writes happened. In a filtered change feed, `has_more`, `next_cursor`, `head` and gaps in the `sequence` and `ordinal` of the remaining events show the same thing. Use separate database files for tenants that must not see each other's write volume. Keys are shared by credentials that can write one scope. The administrator's request log and audit show reserved names. [Memory-bound resources](SECURITY.md#memory-bound-resources) lists all of them.
- **Deletion.** Forget and expiry are logical, and the evidence record of an expired memory stays until it is deleted or expires on its own. Plan backup retention and external copies yourself.
- **Transport.** The clients, the CLI and the MCP tools require HTTPS except for loopback hosts, and never print the token.
