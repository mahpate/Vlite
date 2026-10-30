# Product gaps: vector store, graph store and peer pool

| Field | Value |
|---|---|
| Status | Research for review. Not approved; nothing here is implemented or scheduled |
| Created | 2026-10-29 |
| Scope | What VLite's vector store, property graph and the [peer pool](https://github.com/mahpate/copilot-worker-pool) still lack to form one complete product |
| Related | [`storage-goals.md`](../performance/storage-goals.md) (speed and size), [`feature-agent-graph.prd.md`](../feature-agent-graph/feature-agent-graph.prd.md) (graph scope and deferrals), pool [`TODO.md`](https://github.com/mahpate/copilot-worker-pool/blob/main/TODO.md) |

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
- **Peer pool:** plans with task dependencies (`depends_on`, fan-out and
  fan-in), a planner that splits a request into tasks, a final integration
  review task, a portal and a 7-day retention sweep.

## Ranked gaps

Rank weighs user value against cost. Items that say "Revisits a PRD
deferral" were left out of the graph PRD on purpose; picking one up needs a
new owner decision, and the PRD's original reason still has to be answered.

| # | Gap | Area | Why it matters | Notes and constraints |
|---|---|---|---|---|
| 1 | **Agent memory API**: remember, recall, consolidate and forget, with sessions, users and fact updates | Vector + graph | The main use of a combined vector and graph store today. Mem0 and Zep sell exactly this layer; VLite has the parts but no single API | Built on items 2 and 3. A thin layer over existing graph batches and hybrid retrieval |
| 2 | **TTL for graph nodes and edges** | Graph | The pool wrote its own sweeper (`retention.py`) because the graph cannot expire data | Revisits a PRD deferral (out of scope because independent expiry leaves dangling edges). Needs a rule such as "expiring a node expires its edges", bounded by the 4,096-operation batch limit |
| 3 | **Time-aware graph**: `valid_from`/`valid_to` on edges and "as of" reads | Graph | Lets agents keep fact history instead of overwriting it (Zep/Graphiti's main idea) | Revisits the PRD deferral of advanced temporal modeling. Start with validity only, not full bitemporal |
| 4 | **Graph algorithms**: PageRank, community detection, weighted shortest path, centrality | Graph | Needed for ranking, clustering and GraphRAG summaries | Revisits a PRD deferral (deferred for unbounded cost). Needs edge weights and the same limits and deadlines as traversal (today at most 4 hops, 10,000 visited nodes, 5 s) |
| 5 | **Cypher/GQL subset with writes** | Graph | Most graph users and LLM tools expect openCypher | Extends the read-only [`GRAPH-QUERY.md`](../../GRAPH-QUERY.md). A general-purpose language stays out of scope unless the owner changes that |
| 6 | **Document-to-graph ingestion (GraphRAG)**: extract entities and relationships from text into nodes, edges and linked chunks | Vector + graph | Turns raw documents into a searchable knowledge graph in one step | Needs a pluggable extraction model; never sends data to an external service without explicit configuration |
| 7 | **Server-side reranking and embedding providers** | Vector | Reranker and embedding-provider traits exist only in the Rust API; the server and SDKs can't use them | Configurable per collection, off by default |
| 8 | **Network replication and failover** | Platform | Replicas are file-shipped today and there is one writer per file | Depends on the directory store in [`storage-goals.md`](../performance/storage-goals.md) (WAL and segment shipping) |
| 9 | **Object storage tier** (Azure Blob or S3 segments, read-only serverless readers) | Platform | Lowers cost for large, cold data | Depends on the directory store; Azure Files stays experimental |
| 10 | **Database branching**: cheap copy-on-write forks | Platform | Safe agent experiments and test copies | Natural once segments are immutable |
| 11 | **gRPC graph service** | Graph | gRPC covers vectors only | Revisits a PRD deferral; the REST and MCP graph surfaces already exist |
| 12 | **Document and key-value API** | Vector | Simple metadata and config storage without a vector | Low priority until requested |

Peer-pool gaps are tracked in the pool repository's `TODO.md` under
"Product gaps (2026-10-29 research)". In short:

- a human approval step before a task result is used;
- a per-task trace and cost view;
- reusing past task results as shared memory (already planned there as
  "Add Optional VLite Vector Memory"), ideally on top of item 1;
- replacing the pool's own sweeper with item 2 once it exists.

## Suggested order

1. **Agent memory foundation:** items 2 and 3, then item 1. Small format
   additions to graph frames; the pool is the first user.
2. **Graph depth:** items 4, 5 and 6.
3. **Serving:** items 7 and 11.
4. **Scale-out**, after phase 2 of [`storage-goals.md`](../performance/storage-goals.md):
   items 8, 9 and 10.

Item 12 waits for a concrete request.

## Must not change

- Feature and security gates, the DO-005 quiet-host benchmark policy and
  the frozen `src/openapi.json` stay as they are. New routes need an approved
  OpenAPI change.
- Graph and vector writes stay atomic within a graph batch. New expiry and
  validity rules must not leave dangling edges.
- Benchmarks are run only at major releases.

## Decisions needed

1. Approve the agent memory track (items 1–3) as the next feature work
   alongside the storage goals, or keep storage first.
2. For graph TTL: should an expiring node expire its edges, or should nodes
   with live edges be kept until the edges expire?
3. Whether openCypher with writes (item 5) moves into scope.
4. Which scale-out item (8, 9 or 10) comes first after the directory store.
