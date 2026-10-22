# Named vectors, providers and query language (embedded Rust)

These additive Rust APIs do not change server or binding contracts. Named dense
vectors, sparse values, model history and re-embedding checkpoints require an
explicit `Database::create_with_format(path, dimensions, FormatVersion::V5)`.
The default creation format remains v3. Existing v1–v4 encodings are unchanged.

## Named and versioned vectors

The legacy `Record::vector` remains fixed-dimensional and all existing methods
keep their semantics. Additional vectors attach to an existing, **live** record.
Use `db.collection_mut("default")` (or a named collection) for mutations and
`db.collection(name)` for reads:

```rust
use vlite::{Database, DistanceMetric, FormatVersion};
use vlite::vectors::VectorSpec;
fn example() -> vlite::Result<()> {
let mut db = Database::create_with_format("multimodal.vlite", 2, FormatVersion::V5)?;
db.put(1, &[1.0, 0.0], b"document")?;
let mut collection = db.collection_mut("default")?;
collection.configure_vector("text", VectorSpec {
    dimensions: 3,
    metric: DistanceMetric::Cosine,
    active_version: "model-v1".into(),
    history: 2,
})?;
collection.put_vector_version(1, "text", "model-v1", vec![1.0, 0.0, 0.0])?;
let hits = collection.view().search_named("text", None, &[1.0, 0.0, 0.0], 10)?;
Ok(())
}
```

Each collection may declare at most 32 vector names with independent dimensions
and metrics. `configure_vector` creates an immutable declaration; it refuses
redefinition. `vector_specs` and `record_vectors` inspect declarations and data.
`put_vectors` replaces all additional dense/sparse values of one record
atomically. `put_vector_version` updates one named model version.

`search_named(name, None, query, k)` uses the active model; passing `Some(version)`
selects a retained historical or staged model. Missing versions produce no hits.
`history` retains 0–64 **previous** versions, in addition to the active version
and a protected in-progress re-embedding target. `prune_vector_history` changes
retention and durably prunes oldest inserted history. Version strings are opaque,
nonempty UTF-8 strings up to 256 bytes. A version upsert replaces that version,
not an immutable audit event.

`rebuild_named_hnsw` builds a separate graph per `(name, version)`, for L2/cosine;
inner-product named search remains exact. `search_named_ann` is ready-only and
requires `k <= ef <= MAX_EF_SEARCH`, with positive ef. Graphs are never persisted;
reopen or a mutation invalidates them. Expiry is checked at query time, including
when a graph was built before expiry. `search_named_at` and `search_named_ann_at`
accept an explicit expiry clock for deterministic queries. Base `put`/delete/batch replacement clears
additional vectors for that ID to prevent stale embeddings. Metadata-only updates
retain active/history embeddings, but invalidate staged re-embedding results.

## Sparse and hybrid search

Populate `RecordVectors::sparse` with a map from name to sorted `(u32, f32)` pairs.
Indices must be unique and increasing; values must be finite; empty vectors are
valid, with at most 65,536 entries per sparse vector and 32 sparse names per record.
Sparse names are independent of dense declarations. `search_sparse` uses a derived
inverted index rebuilt on open and updated after successful mutations. The distance
is **negative dot product**, so smaller is better. Zero-overlap records participate:
a zero score outranks a negative dot product. ID breaks ties.

`search_dense_sparse(&DenseSparseQuery)` combines independently retrieved dense
and sparse candidates using the shared `hybrid::FusionOptions` and
`hybrid::fuse_ranked` weighted reciprocal rank fusion. `vector_weight` weights
the dense leg and `text_weight` weights the sparse leg; `text_rank` identifies the
sparse rank in the reused `HybridHit` result. Defaults are equal weights and
`rrf_k=60`. The existing lexical/vector `search_hybrid` API is unchanged. Candidate
depth and result k are bounded by 1,000. Fusion scores sort descending, then by ID.

## Resumable background re-embedding

1. `begin_reembed(name, target_version)` creates a durable per-name job, leaving
   the old active version visible to every ordinary search.
2. `reembed_pending_checked(name, limit)` returns IDs, source `Record`s and source
   revisions. Release the owner's application lock while calling your embedder.
3. `apply_reembed_batch_checked(name, &[ReembedUpdate])` validates source revisions,
   then atomically stores all vectors and the cursor. A changed source rejects the
   entire batch with `RevisionMismatch`. Retry with fresh pending records.
4. Repeat until pending is empty; `finish_reembed(name)` verifies completeness
   against **current live records**, atomically switches the active version and
   applies history pruning. Incomplete jobs cannot switch.

`reembed_status` exposes the persisted target and last processed ID. The cursor is
an observability/checkpoint high-water mark, not an exclusion boundary: the pending
iterator also finds new/changed IDs below it. Deletion/expiry does not block
completion. Reopen and compaction preserve both staged vectors and checkpoints.
`reembed_pending`/`apply_reembed_batch` are lower-level unchecked equivalents for
callers that guarantee the source is unchanged. Batches are idempotent, 1–4,096
entries. Execution scheduling, worker threads and provider retry/backoff belong
to the embedding application; the engine performs no hidden background network
calls. Reads continue to use the old active embeddings throughout the job.

## Embedding providers

`embedding::Embedder` is a `Send + Sync` trait with `embed_bytes` and `embed_text`.
`EmbedderRegistry` registers application-owned providers by name as `Arc<dyn
Embedder>`. `HashEmbedder::new(dimensions)` is deterministic byte/n-gram hashing,
unit-normalized for nonzero outputs (empty input is zero); it is intended for tests
and pipelines, **not semantic quality**. No ONNX or other model runtime is linked.

Feature `http-embedder` enables a blocking OpenAI-compatible `HttpEmbedder` using
reqwest/rustls. `new(url, model, optional_key)` requires the full embeddings URL,
e.g. `https://api.openai.com/v1/embeddings`. `from_env` reads
`VLITE_EMBEDDING_URL`, `VLITE_EMBEDDING_MODEL`, optional `VLITE_EMBEDDING_KEY`.
Inputs are UTF-8, the request uses `model` and `input`, and the first
`data[0].embedding` is validated for dimensions and finite values. HTTPS verifies
certificates; HTTP is supported for local deployments/tests. Redirects are disabled,
the request timeout is 30 seconds and responses are capped at 4 MiB. Errors do not
include keys, request bodies or endpoint responses. Never call a blocking provider
on an async runtime worker; use a blocking worker. Integration tests use only local
loopback HTTP servers, never external providers.

## Re-ranking

`rerank::Reranker` receives a query and top-N `RerankCandidate`s, including
full-precision vectors and payloads. Implement it for a cross-encoder/provider.
`CollectionRef::rerank(query, candidates, limit, provider)` resolves current live
base records (at most 4,096 candidates) before invoking the provider.
`ExactReranker { metric }` re-scores full-precision vectors and sorts by distance/ID;
`MmrReranker { lambda }` greedily maximizes cosine relevance minus redundancy.
Lambda must be finite in `[0,1]`, with 1 meaning relevance only. A cosine zero-norm
query is rejected; zero-norm candidates are omitted. The existing quantized ANN
already performs exact candidate re-scoring; this trait adds pluggable postprocessing
rather than replacing that behavior. Named-vector callers can construct candidates
from `record_vectors` and invoke the same trait directly.

## Parsed, read-only queries

`query_language::parse` returns `ParsedQuery` or `ParseError { position, message }`
with a byte position. `ParsedQuery::execute(collection, &BTreeMap<String, Vec<f32>>)`
returns hits, optional groups and optional facet counts. Only this grammar is
accepted; no scripts, writes, arbitrary expressions or additional statements run:

```text
SEARCH <vector-name> NEAR $parameter [VERSION model-version] [TEXT "lexical query"]
  [K number] [WHERE predicate [AND predicate ...]]
  [GROUP BY field] [FACET [BY] field] [ORDER BY field [ASC|DESC]] [LIMIT number]

SEARCH TEXT MATCH "lexical query" [K number] ...same trailing clauses...
```

`default` targets the unversioned legacy vector; all other vector names must have
been declared. Identifiers use letters/digits/underscore/dot/hyphen. Keywords are
case-insensitive, names/values are case-sensitive. Parameters bind vectors only;
quoted strings support escaped quote, backslash, newline and tab. Scalar values
are strings, i64 integers, finite floats or booleans. Numeric typing remains strict:
integer `20` and float `20.0` are distinct, as in existing metadata filters.

Predicates: `field = value`, `field < value`, `<=`, `>`, `>=`,
`field IN (value, ...)`, `EXISTS field` (or `field EXISTS`), and
`GEO(field, latitude, longitude, radius_meters)`. Ranges may be expressed with two
AND predicates. GEO maps to the existing validated `Condition::GeoRadius`.
Grouping uses matching metadata values and retains ordered hits per group. Facets
count the matching retrieval set **before LIMIT**. Ordering accepts metadata fields,
`distance` or `id`, breaking ties by ID. Missing group/facet fields are omitted.

Queries are bounded to 16 KiB, 64 predicates, 64 IN values, and limit/k 0–1,000.
Exact vector and text execution materialize their matching candidate sets; this is
a small embedded query interface, not an optimizing SQL engine. Mixed dense/text
queries fuse the first 1,000 filtered candidates per leg with weighted RRF; their
groups/facets describe that fused candidate set, not all collection records.
Text queries require declared, available `text_fields`. Unsupported clauses and
multi-statement input are errors, never silently executed.

## Storage and compatibility limits

V5 operation **10** is an atomic collection-vector-state replacement. It includes
all declarations, additional dense vectors/model histories, sparse values and
re-embedding checkpoints in a bounded binary body; the ordinary frame header and
commit footer checksums cover every byte. The body is capped at **16 MiB per
collection snapshot**, so large histories/additional vectors may need pruning or
multiple collections. This first implementation rewrites that snapshot per vector
mutation (write amplification); it is not intended for unbounded model histories.
Sparse postings and named HNSW graphs are derived, not stored. Compaction preserves
live additional vectors/history/checkpoints and omits expired records. Backup and
restore retain the bytes as usual. Explicit incomplete-tail recovery discards only
a structurally interrupted final frame; complete corruption is rejected. V1–V4
files never gain operation 10. Older releases reject the opt-in v5 header rather
than interpreting new records. Existing size/inspection statistics describe base
record bodies, not additional-vector snapshots.
