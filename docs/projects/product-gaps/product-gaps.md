# Product gaps: vector store, graph store and peer pool

| Field | Value |
|---|---|
| Status | Approved 2026-10-30 as the feature track of [`next-major.md`](../roadmap/next-major.md); nothing implemented yet |
| Created | 2026-10-29 |
| Scope | What VLite's vector store, property graph and the peer pool (a private, owner-only project) still lack to form one complete product |
| Related | [`storage-goals.md`](../performance/storage-goals.md) (speed and size), [`feature-agent-graph.prd.md`](../feature-agent-graph/feature-agent-graph.prd.md) (graph scope and deferrals) |

## Method

We compared VLite 0.6.1 against about 15 vector, graph and agent-memory
products: Qdrant, Milvus, Weaviate, LanceDB, Chroma, pgvector, zvec, Neo4j,
Memgraph, Kuzu, FalkorDB, SurrealDB, Mem0, Zep/Graphiti and Letta. Each gap
was then checked against VLite's own sources: `src/openapi.json`, the README
"Status and limitations" section, [`GRAPH.md`](../../GRAPH.md),
[`GRAPH-QUERY.md`](../../GRAPH-QUERY.md) and
[`CAPABILITIES.md`](../../CAPABILITIES.md). Gaps that VLite already covers
were removed.

## Already covered (not gaps)

- **Vectors:** dense, sparse and multi-vector search; named vectors; quantized
  ANN with re-ranking; disk ANN; BM25 hybrid search with RRF fusion;
  recommend, geo, grouped and facet queries; record expiry (V5 files).
- **Graph:** schemas with property indexes, conditional writes
  (`expected`), unweighted shortest path, hybrid retrieval
  (`GraphRef::retrieve`), a read-only query language and MCP graph tools.
- **Operations:** tenants, users, scoped API keys, TLS reload, webhooks, change
  feed, scheduled and verified backups, restore drills, point-in-time
  restore, encryption, slow-query diagnostics and an MCP server.
- **Conditional writes:** `If-Match` on vector records and `expected` on
  graph writes.
- **Peer pool:** plans with task dependencies (`depends_on`, fan-out and
  fan-in), a planner that splits a request into tasks, a final integration
  review task, a portal and a 7-day retention sweep.

## Ranked gaps

Rank weighs user value against cost. Items that say "Revisits a PRD
deferral" were left out of the graph PRD on purpose; picking one up needs a
new owner decision, and the PRD's original reason still has to be answered.

| # | Gap | Area | Why it matters | Notes and constraints |
|---|---|---|---|---|
| 1 | **Agent memory API**: remember, recall, consolidate and forget, with sessions, users and fact updates | Vector + graph | The main use of a combined vector and graph store today. Mem0 and Zep sell exactly this layer; VLite has the parts but no single API | Built on items 2, 3 and 4. A thin layer over existing graph batches and hybrid retrieval |
| 2 | **Key-value API**: string keys in namespaces, values without a vector, optional expiry and conditional writes | Vector + graph | The base store under most agent frameworks (LangGraph `BaseStore`, LlamaIndex KV stores). Also fits the pool's task claims, heartbeats and idempotency keys | Reuses existing parts: payloads, record expiry (V5 files), `If-Match` and graph `expected` checks. Missing: string keys, namespaces and vector-free records. Conditional writes alone do not make leases safe; claims also need fencing tokens. New routes need an approved OpenAPI change |
| 3 | **TTL for graph nodes and edges** | Graph | The pool wrote its own sweeper (`retention.py`) because the graph cannot expire data | Revisits a PRD deferral (out of scope because independent expiry leaves dangling edges). Needs a rule such as "expiring a node expires its edges", bounded by the 4,096-operation batch limit |
| 4 | **Time-aware graph**: `valid_from`/`valid_to` on edges and "as of" reads | Graph | Lets agents keep fact history instead of overwriting it (Zep/Graphiti's main idea) | Revisits the PRD deferral of advanced temporal modeling. Start with validity only, not full bitemporal |
| 5 | **Graph algorithms**: PageRank, community detection, weighted shortest path, centrality | Graph | Needed for ranking, clustering and GraphRAG summaries | Revisits a PRD deferral (deferred for unbounded cost). Needs edge weights and the same limits and deadlines as traversal (today at most 4 hops, 10,000 visited nodes, 5 s) |
| 6 | **Cypher/GQL subset with writes** | Graph | Most graph users and LLM tools expect openCypher | Extends the read-only [`GRAPH-QUERY.md`](../../GRAPH-QUERY.md). A general-purpose language stays out of scope unless the owner changes that |
| 7 | **Document-to-graph ingestion (GraphRAG)**: extract entities and relationships from text into nodes, edges and linked chunks | Vector + graph | Turns raw documents into a searchable knowledge graph in one step | Needs a pluggable extraction model; never sends data to an external service without explicit configuration |
| 8 | **Server-side reranking and embedding providers** | Vector | Reranker and embedding-provider traits exist only in the Rust API; the server and SDKs can't use them | Configurable per collection, off by default |
| 9 | **Network replication and failover** | Platform | Replicas are file-shipped today and there is one writer per file | Depends on the directory store in [`storage-goals.md`](../performance/storage-goals.md) (WAL and segment shipping) |
| 10 | **Object storage tier** (Azure Blob or S3 segments, read-only serverless readers) | Platform | Lowers cost for large, cold data | Depends on the directory store; Azure Files stays experimental |
| 11 | **Database branching**: cheap copy-on-write forks | Platform | Safe agent experiments and test copies | Natural once segments are immutable |
| 12 | **gRPC graph service** | Graph | gRPC covers vectors only | Revisits a PRD deferral; the REST and MCP graph surfaces already exist |

Peer-pool gaps are tracked in the pool repository's `TODO.md` under
"Product gaps (2026-10-29 research)". In short:

- a human approval step before a task result is used;
- a per-task trace and cost view;
- reusing past task results as shared memory (already planned there as
  "Add Optional VLite Vector Memory"), ideally on top of item 1;
- storing task claims and heartbeats with item 2;
- replacing the pool's own sweeper with item 3 once it exists.

## Suggested order

1. **Agent memory foundation:** item 2, then items 3 and 4, then item 1.
   Mostly API work over existing records plus small graph-frame additions;
   the pool is the first user.
2. **Graph depth:** items 5, 6 and 7.
3. **Serving:** items 8 and 12.
4. **Scale-out**, after phase 2 of [`storage-goals.md`](../performance/storage-goals.md):
   items 9, 10 and 11.

## Considered and deferred

- **Append-only log or queue** (claim, acknowledge, retry): overlaps the
  change feed and would make VLite a message broker. The pool's claims fit
  item 2 instead.
- **Document store with secondary indexes:** mostly covered by typed
  metadata filters and facets. Revisit as vector-free collections once
  item 2 exists.
- **SQL with joins**, **time series** and a **separate blob store**: little
  evidence of demand from agent workloads.

## Must not change

- Feature and security gates, the DO-005 quiet-host benchmark policy and
  the frozen `src/openapi.json` stay as they are. New routes need an approved
  OpenAPI change.
- Graph and vector writes stay atomic within a graph batch. New expiry and
  validity rules must not leave dangling edges.
- Benchmarks are run only at major releases.

## Decisions

1. **Approved:** the agent memory track (items 1–4) runs alongside the
   storage goals, starting with the key-value API (item 2).
2. **Open:** for graph TTL, does an expiring node expire its edges, or are
   nodes with live edges kept until the edges expire? Needed before item 3.
3. **Approved:** openCypher with writes (item 6) is in scope, with graph
   depth (items 5–7).
4. **Open:** which scale-out item (9, 10 or 11) comes first after the
   directory store.
