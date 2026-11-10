//! REG-008 surface parity (feature-agent-graph EPIC-001 ITEM-002/ITEM-003).
//!
//! Compares the current build's served `/openapi.json` bytes (modulo the
//! exact, reviewed edits in `APPROVED_OPENAPI_EDITS` and additions in
//! `APPROVED_OPENAPI_ADDITIONS`) and `vlite-mcp`
//! responses (read-only and writable) on a vector-only file against the golden
//! captures produced from baseline commit `347b01d` by
//! `python scripts\graph_baseline.py fixtures`, except for the package version
//! string, which release preparation bumps (see `MASKED_VERSION`). Requires
//! `server` and `mcp`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use vlite::Database;
use vlite::server::{Service, router};

fn fixture(name: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("graph_baseline")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .replace("\r\n", "\n")
}

/// Reviewed in-line edits to existing baseline lines, applied to the capture
/// before comparison as `(baseline text, served text)`; each baseline text
/// must occur exactly once. v1.2.24/v1.2.25 (EPIC-016, graph/vector feature
/// parity): `/v1/changes` documents the graph event types, and
/// `/v1/admin/webhooks` makes `events` optional (every event by default) and
/// adds the `graphs` filter.
/// Private Gap 8 REST step 2a (owner-approved 2026-11-03): existing embed and
/// search routes gain managed aliases, custom ranking, typed responses and
/// the seven approved provider error codes; custom k is bounded to 1..=1000.
const APPROVED_OPENAPI_EDITS: &[(&str, &str)] = &[
    // Ordinary KV durability qualifier; every other baseline byte is retained.
    (
        r##"    "description": "Experimental API for one exclusively owned database. Bearer authentication is required except explicit loopback-only development mode. Optional distinct read-only credentials permit GET/HEAD outside /v1/admin/ and non-building collection search/retrieval POSTs; lazy approximate index builds and other writes require admin or an appropriately scoped credential. /health and static dashboard assets are public. Use --tls-cert/--tls-key for native HTTPS or terminate TLS at a trusted proxy. --grpc-tls-client-ca enables mandatory gRPC client certificates. Admin TLS reload validates all listeners before replacing certificates. Optional tenant-bound credentials, request rate limits, live-vector quotas and OTLP tracing are disabled unless configured. No CORS, automatic repair or retries. X-Request-ID and Cache-Control: no-store are included. IDs and administrative seeds use decimal strings; payloads use padded standard base64. JSON is limited to 8 MiB and 16 in-flight requests/workers. Successful writes are synced. Disconnects and commit_indeterminate errors do not prove writes failed; never blindly retry. Backups require VLITE_ADMIN_DIR; restore selection takes effect only on restart and never replaces the live owner. Token reload reads only configured files. See docs/CONSOLE-ADMIN.md for semantics, bounds and durability limitations. Protected operations may return 401, 403, 422, 429 (with Retry-After) or 503 with the Error schema.""##,
        r##"    "description": "Experimental API for one exclusively owned database. Bearer authentication is required except explicit loopback-only development mode. Optional distinct read-only credentials permit GET/HEAD outside /v1/admin/ and non-building collection search/retrieval POSTs; lazy approximate index builds and other writes require admin or an appropriately scoped credential. /health and static dashboard assets are public. Use --tls-cert/--tls-key for native HTTPS or terminate TLS at a trusted proxy. --grpc-tls-client-ca enables mandatory gRPC client certificates. Admin TLS reload validates all listeners before replacing certificates. Optional tenant-bound credentials, request rate limits, live-vector quotas and OTLP tracing are disabled unless configured. No CORS, automatic repair or retries. X-Request-ID and Cache-Control: no-store are included. IDs and administrative seeds use decimal strings; payloads use padded standard base64. JSON is limited to 8 MiB and 16 in-flight requests/workers. Successful ordinary KV writes follow configured durability: full syncs, periodic can lose one flush interval, and none is memory-only; they do not force a sync. Disconnects and commit_indeterminate errors do not prove writes failed; never blindly retry. Backups require VLITE_ADMIN_DIR; restore selection takes effect only on restart and never replaces the live owner. Token reload reads only configured files. See docs/CONSOLE-ADMIN.md for semantics, bounds and durability limitations. Protected operations may return 401, 403, 422, 429 (with Retry-After) or 503 with the Error schema.""##,
    ),
    (
        r##"    "/v1/collections/{collection}/embed":{"parameters":[{"name":"collection","in":"path","required":true,"schema":{"type":"string"}}],"post":{"operationId":"EmbedText","summary":"Generate a collection-dimension text vector with a configured provider","description":"The deterministic hashing provider is built in. provider=http uses the server's VLITE_EMBEDDING_URL, VLITE_EMBEDDING_MODEL and optional VLITE_EMBEDDING_KEY when built with http-embedder. Text is sent only to the explicitly configured HTTP provider.","requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/EmbedRequest"}}}},"responses":{"200":{"description":"Embedding vector and provider"},"409":{"description":"HTTP provider is disabled in this build"},"422":{"description":"Invalid text/provider or dimension mismatch"}}}},"##,
        r##"    "/v1/collections/{collection}/embed":{"parameters":[{"name":"collection","in":"path","required":true,"schema":{"type":"string"}}],"post":{"operationId":"EmbedText","summary":"Generate a collection-dimension text vector with a configured provider","description":"The deterministic hashing provider is built in. provider=http uses the server's VLITE_EMBEDDING_URL, VLITE_EMBEDDING_MODEL and optional VLITE_EMBEDDING_KEY when built with http-embedder. Text is sent only to the explicitly configured HTTP provider. Configured aliases must be in the administrator-controlled allowlist for this collection, and the caller must have search authorization. External calls require an explicitly configured endpoint and egress policy; the alias is pinned to its configured model/version and dimensions. One UTF-8 text is limited to 16,384 bytes; encoded requests are limited to 1 MiB and responses to 4 MiB, with 2 s connect and 10 s total deadlines and bounded admission. This contract does not enable providers by default.","requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/EmbedRequest"}}}},"responses":{"200":{"description":"Embedding vector and provider","content":{"application/json":{"schema":{"$ref":"#/components/schemas/EmbedResponse"}}}},"409":{"description":"HTTP provider is disabled in this build; provider_not_configured indicates the requested alias is absent or not enabled for this collection. A provider_snapshot_conflict means the serving snapshot changed during the request; the response is sanitized with no candidate, tenant, or provider data, has no Retry-After, and callers retry the whole request.","content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderNotConfiguredError"},{"$ref":"#/components/schemas/ProviderSnapshotConflictError"}]}}}},"422":{"description":"Invalid text/provider or dimension mismatch; invalid_provider_input identifies provider-specific input or custom candidate depth.","content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderInvalidInputError"}]}}}},"401":{"$ref":"#/components/responses/Unauthorized"},"403":{"description":"forbidden_scope: the caller is not authorized to search this collection or use the configured provider alias.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"502":{"description":"provider_invalid_response: provider response data is malformed or has invalid dimensions, cardinality, IDs, or scores.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/ProviderInvalidResponseError"}}}},"503":{"description":"Existing service-unavailable behavior remains; provider_unavailable covers provider connection, authentication, or availability failures.","content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderUnavailableError"}]}}}},"504":{"description":"provider_timeout: the provider call exceeded its total deadline.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/ProviderTimeoutError"}}}},"429":{"description":"provider_busy when provider or tenant admission is full.","headers":{"Retry-After":{"description":"Retry-After is supplied for provider_busy and existing credential/quota throttling.","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderBusyError"}]}}}}}}},"##,
    ),
    (
        r##"    "/v1/collections/{c}/search":{"parameters":[{"name":"c","in":"path","required":true,"schema":{"type":"string"}}],"post":{"summary":"Search a collection using its default settings","requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/SearchRequest"}}}},"responses":{"200":{"description":"Hits; hybrid scores sort descending, other distances ascending. explain adds stats and optional auto plan."},"409":{"description":"Requested ready-only index is unavailable"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        r##"    "/v1/collections/{c}/search":{"parameters":[{"name":"c","in":"path","required":true,"schema":{"type":"string"}}],"post":{"summary":"Search a collection using its default settings","requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/SearchRequest"}}}},"responses":{"200":{"description":"Hits; hybrid scores sort descending, other distances ascending. explain adds stats and optional auto plan.","content":{"application/json":{"schema":{"type":"object","required":["hits"],"properties":{"hits":{"type":"array","items":{"type":"object","required":["id"],"properties":{"id":{"$ref":"#/components/schemas/Id"},"distance":{"type":"number"},"score":{"type":"number"},"rerank_score":{"type":"number","description":"Optional finite custom rerank score (-cost); higher is better."}}}},"ranking":{"type":"object","description":"Present when custom reranking is applied.","additionalProperties":false,"required":["provider","order"],"properties":{"provider":{"$ref":"#/components/schemas/ProviderAlias"},"order":{"enum":["score_desc_id_asc"]}}},"stats":{"$ref":"#/components/schemas/SearchStats"},"plan":{"oneOf":[{"$ref":"#/components/schemas/QueryPlan"},{"type":"null"}]}}}}}},"409":{"description":"Requested ready-only index is unavailable; provider_not_configured identifies an alias absent or disabled for this collection. A provider_snapshot_conflict means the serving snapshot changed during the request; the response is sanitized with no candidate, tenant, or provider data, has no Retry-After, and callers retry the whole request.","content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderNotConfiguredError"},{"$ref":"#/components/schemas/ProviderSnapshotConflictError"}]}}}},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries. Provider or tenant admission full returns provider_busy.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1); provider admission may also return provider_busy.","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderBusyError"}]}}}},"401":{"$ref":"#/components/responses/Unauthorized"},"403":{"description":"forbidden_scope: the caller is not authorized to search this collection or use the configured provider alias.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"422":{"description":"Schema or database validation rejected input; no write. invalid_provider_input covers invalid provider input or custom candidate depth.","content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderInvalidInputError"}]}}}},"502":{"description":"provider_invalid_response: provider response data is malformed or has invalid dimensions, cardinality, IDs, or scores.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/ProviderInvalidResponseError"}}}},"503":{"description":"Existing service-unavailable behavior remains; provider_unavailable covers provider connection, authentication, or availability failures.","content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderUnavailableError"}]}}}},"504":{"description":"provider_timeout: the provider call exceeded its total deadline.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/ProviderTimeoutError"}}}}},"description":"Search a collection using its default settings. Configured provider aliases must be administrator-allowlisted for the collection, and the caller must have search authorization before alias resolution or egress. Requests cannot supply endpoints or models. External calls require explicit administrator endpoint and egress configuration, with pinned model/version and bounded admission. Provider requests are limited to 1 MiB encoded and responses to 4 MiB; connect and total deadlines are 2 s and 10 s. These bounds do not enable providers by default. Custom reranking requires 1 <= k <= candidates <= 1000; when candidates is omitted it defaults to max(k,50), while exact/MMR defaults remain unchanged."}},"##,
    ),
    (
        r##"    "/v1/search": {"post":{"summary":"Search the default collection with legacy parameter defaults","description":"Supports exact, approximate, approximate_ready, auto and hybrid modes. Distance modes order ascending then numeric ID; hybrid ranks RRF scores descending. Auto never builds and may choose an indexed exact scan, full exact scan or ready ANN. Hybrid requires v4 and declared text fields for its nonzero text leg. Exact supports squared_l2, cosine and inner_product; ANN supports squared_l2/cosine. Strict-type AND filters are validated before readiness or empty-query handling. Nonmatching ANN nodes remain traversal bridges; ef_search bounds retained eligible candidates, not visited nodes or time. ANN has no hidden exact fallback. Scalar-u8 traversal reranks with current f32 vectors and does not promise total RAM savings. Lazy approximate may build synchronously: read-only credentials cannot build, and scoped credentials additionally need search_build. approximate_ready returns 409 on missing or mismatched index identity. Use collection routes to inherit persisted settings instead of this alias's hard defaults. explain adds work counters and an auto plan. See docs/CAPABILITIES.md for bounds and score semantics.","requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/SearchRequest"}}}},"responses":{"200":{"description":"Hits ordered by distance ascending or hybrid score descending, then numeric ID. explain includes stats and optional plan.","content":{"application/json":{"schema":{"type":"object","required":["hits"],"properties":{"hits":{"type":"array","items":{"type":"object","required":["id"],"properties":{"id":{"$ref":"#/components/schemas/Id"},"distance":{"type":"number"},"score":{"type":"number"}}}},"stats":{"$ref":"#/components/schemas/SearchStats"},"plan":{"oneOf":[{"$ref":"#/components/schemas/QueryPlan"},{"type":"null"}]}}}}}},"409":{"description":"index_not_ready in approximate_ready mode; explicitly build the index or choose another mode","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"400":{"$ref":"#/components/responses/BadRequest"},"413":{"$ref":"#/components/responses/TooLarge"},"401":{"$ref":"#/components/responses/Unauthorized"},"415":{"$ref":"#/components/responses/MediaType"},"422":{"$ref":"#/components/responses/InvalidInput"},"503":{"$ref":"#/components/responses/Unavailable"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        r##"    "/v1/search": {"post":{"summary":"Search the default collection with legacy parameter defaults","description":"Supports exact, approximate, approximate_ready, auto and hybrid modes. Distance modes order ascending then numeric ID; hybrid ranks RRF scores descending. Auto never builds and may choose an indexed exact scan, full exact scan or ready ANN. Hybrid requires v4 and declared text fields for its nonzero text leg. Exact supports squared_l2, cosine and inner_product; ANN supports squared_l2/cosine. Strict-type AND filters are validated before readiness or empty-query handling. Nonmatching ANN nodes remain traversal bridges; ef_search bounds retained eligible candidates, not visited nodes or time. ANN has no hidden exact fallback. Scalar-u8 traversal reranks with current f32 vectors and does not promise total RAM savings. Lazy approximate may build synchronously: read-only credentials cannot build, and scoped credentials additionally need search_build. approximate_ready returns 409 on missing or mismatched index identity. Use collection routes to inherit persisted settings instead of this alias's hard defaults. explain adds work counters and an auto plan. See docs/CAPABILITIES.md for bounds and score semantics. Configured provider aliases must be administrator-allowlisted for the collection, and the caller must have search authorization before alias resolution or egress. Requests cannot supply endpoints or models. External calls require explicit administrator endpoint and egress configuration, with pinned model/version and bounded admission. Provider requests are limited to 1 MiB encoded and responses to 4 MiB; connect and total deadlines are 2 s and 10 s. These bounds do not enable providers by default. Custom reranking requires 1 <= k <= candidates <= 1000; when candidates is omitted it defaults to max(k,50), while exact/MMR defaults remain unchanged.","requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/SearchRequest"}}}},"responses":{"200":{"description":"Hits ordered by distance ascending or hybrid score descending, then numeric ID. explain includes stats and optional plan.","content":{"application/json":{"schema":{"type":"object","required":["hits"],"properties":{"hits":{"type":"array","items":{"type":"object","required":["id"],"properties":{"id":{"$ref":"#/components/schemas/Id"},"distance":{"type":"number"},"score":{"type":"number"},"rerank_score":{"type":"number","description":"Optional finite custom rerank score (-cost); higher is better."}}}},"stats":{"$ref":"#/components/schemas/SearchStats"},"plan":{"oneOf":[{"$ref":"#/components/schemas/QueryPlan"},{"type":"null"}]},"ranking":{"type":"object","description":"Present when custom reranking is applied.","additionalProperties":false,"required":["provider","order"],"properties":{"provider":{"$ref":"#/components/schemas/ProviderAlias"},"order":{"enum":["score_desc_id_asc"]}}}}}}}},"409":{"description":"index_not_ready in approximate_ready mode; explicitly build the index or choose another mode; provider_not_configured identifies an alias absent or disabled for this collection. A provider_snapshot_conflict means the serving snapshot changed during the request; the response is sanitized with no candidate, tenant, or provider data, has no Retry-After, and callers retry the whole request.","content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderNotConfiguredError"},{"$ref":"#/components/schemas/ProviderSnapshotConflictError"}]}}}},"400":{"$ref":"#/components/responses/BadRequest"},"413":{"$ref":"#/components/responses/TooLarge"},"401":{"$ref":"#/components/responses/Unauthorized"},"415":{"$ref":"#/components/responses/MediaType"},"422":{"description":"Schema or database validation rejected input; no write. invalid_provider_input covers invalid provider input or custom candidate depth.","content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderInvalidInputError"}]}}}},"503":{"description":"Busy, closed, poisoned, or indeterminate commit. commit_indeterminate can mean the write succeeded. Do not blindly retry mutations. provider_unavailable covers provider connection, authentication, or availability failures.","content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderUnavailableError"}]}}}},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries. Provider or tenant admission full returns provider_busy.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1); provider admission may also return provider_busy.","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"anyOf":[{"$ref":"#/components/schemas/Error"},{"$ref":"#/components/schemas/ProviderBusyError"}]}}}},"403":{"description":"forbidden_scope: the caller is not authorized to search this collection or use the configured provider alias.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"502":{"description":"provider_invalid_response: provider response data is malformed or has invalid dimensions, cardinality, IDs, or scores.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/ProviderInvalidResponseError"}}}},"504":{"description":"provider_timeout: the provider call exceeded its total deadline.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/ProviderTimeoutError"}}}}}}},"##,
    ),
    (
        r##"        "allOf":[{"if":{"properties":{"mode":{"const":"hybrid"}},"required":["mode"]},"then":{"properties":{"k":{"maximum":1000}}},"else":{"required":["vector"]}}]"##,
        r##"        "allOf":[{"if":{"properties":{"mode":{"const":"hybrid"}},"required":["mode"]},"then":{"properties":{"k":{"maximum":1000}}},"else":{"required":["vector"]}},{"if":{"properties":{"rerank":{"properties":{"mode":{"const":"custom"}},"required":["mode"]}},"required":["rerank"]},"then":{"properties":{"k":{"minimum":1,"maximum":1000}}}}]"##,
    ),
    (
        r##"      "RerankOptions":{"type":"object","additionalProperties":false,"required":["mode"],"properties":{"mode":{"enum":["exact","mmr"]},"candidates":{"type":"integer","minimum":1,"maximum":1000,"description":"Must be at least k; defaults to max(k,50)."},"lambda":{"type":"number","minimum":0,"maximum":1,"description":"MMR relevance weight (default 0.5); only valid with mode=mmr."}}},"##,
        r##"      "RerankOptions":{"type":"object","additionalProperties":false,"required":["mode"],"properties":{"mode":{"enum":["exact","mmr","custom"]},"candidates":{"type":"integer","minimum":1,"maximum":1000,"description":"Must be at least k; defaults to max(k,50). Custom mode requires 1 <= k <= candidates <= 1000."},"lambda":{"type":"number","minimum":0,"maximum":1,"description":"MMR relevance weight (default 0.5); only valid with mode=mmr."},"provider":{"$ref":"#/components/schemas/ProviderAlias","description":"Configured provider alias; required only when mode is custom."}},"allOf":[{"if":{"properties":{"mode":{"const":"custom"}},"required":["mode"]},"then":{"required":["provider"]},"else":{"not":{"required":["provider"]}}},{"if":{"properties":{"mode":{"not":{"const":"mmr"}}},"required":["mode"]},"then":{"not":{"required":["lambda"]}}}]},"##,
    ),
    (
        r##"      "EmbedRequest":{"type":"object","additionalProperties":false,"required":["text"],"properties":{"text":{"type":"string","maxLength":16384},"provider":{"enum":["hashing","http"],"default":"hashing"}}},"##,
        r##"      "EmbedRequest":{"type":"object","additionalProperties":false,"required":["text"],"properties":{"text":{"type":"string","maxLength":16384},"provider":{"allOf":[{"$ref":"#/components/schemas/ProviderAlias"}],"default":"hashing","description":"Built-in hashing, legacy http, or a configured alias authorized for this collection."}}},"##,
    ),
    (
        r##"      "Error": {"type": "object", "required": ["error"], "properties": {"error": {"type": "object", "required": ["code", "message"], "properties": {"code": {"type": "string"}, "message": {"type": "string"}}}}}"##,
        r##"      "Error": {"type": "object", "required": ["error"], "properties": {"error": {"type": "object", "required": ["code", "message"], "properties": {"code": {"type": "string"}, "message": {"type": "string"}}}}},"##,
    ),
    (
        r##"ween pages"},"400":{"##,
        r##"ween pages. Each event has sequence, ordinal, collection, committed_at_ms and a change object whose type is put, delete, metadata_update, collection_created, collection_dropped, settings_updated or vectors_updated (vector events), or graph_created, graph_dropped or graph_batch (graph-enabled files; collection is empty). Graph events carry graph; graph_batch also carries revision (decimal string) and the ordered operations of one graph frame: node_upserted, node_patched, node_deleted, edge_upserted, edge_patched, edge_deleted, attached, detached or schema_updated, with u64 IDs and records as decimal strings. Scoped credentials see graph events only for graphs their graph_read binding allows."},"400":{"##,
    ),
    (
        r##"events","secret"],"properties":{"url":{"type":"string","format":"uri"},"collections":{"type":"array","items":{"type":"string"}},"events":{"type":"array","items":{"type":"string"}},"secret":{"type":"string","minLength":16,"maxLength":256}}}}}},"responses":{"201":{"description":"Created subscription; secret is not returned"},"409":{"description":"VLITE_ADMIN_DIR is required"},"422":{"description":"Invalid URL, event"##,
        r##"secret"],"properties":{"url":{"type":"string","format":"uri"},"collections":{"type":"array","items":{"type":"string"},"description":"Optional filter for vector events; graph events ignore it"},"events":{"type":"array","minItems":1,"maxItems":10,"items":{"enum":["put","delete","metadata_update","collection_created","collection_dropped","settings_updated","vectors_updated","graph_created","graph_dropped","graph_batch"]},"description":"Optional event types to deliver; omitted means every vector and graph event"},"graphs":{"type":"array","maxItems":64,"items":{"type":"string","minLength":1,"maxLength":63},"description":"Optional filter for graph events; omitted means every graph"},"secret":{"type":"string","minLength":16,"maxLength":256}}}}}},"responses":{"201":{"description":"Created subscription; secret is not returned"},"409":{"description":"VLITE_ADMIN_DIR is required"},"422":{"description":"Invalid URL, event, graph filter"##,
    ),
    (
        "Successful ordinary KV writes follow configured durability: full syncs, periodic can lose one flush interval, and none is memory-only; they do not force a sync.",
        "Successful ordinary KV writes follow configured durability: full syncs, periodic can lose one flush interval, and none is memory-only; they do not force a sync. Successful mutations are acknowledged after commit.",
    ),
    // Metrics counter for KV puts that found all 16 probe slots occupied.
    (
        r##""quota_exceeded_total":{"type":"integer","minimum":0}}},"##,
        r##""quota_exceeded_total":{"type":"integer","minimum":0},"kv_slots_exhausted_total":{"type":"integer","minimum":0,"description":"Requests, REST or gRPC, that failed with kv_slots_exhausted: all 16 probe slots of the key held live entries or other records"}}},"##,
    ),
    // Record puts refuse an expiry that is not later than the server clock.
    (
        r##""expires_at_ms": {"type": ["integer", "null"], "minimum": 0}, "ttl_ms": {"type": "integer", "minimum": 0, "description": "Relative lifetime in milliseconds; mutually exclusive with expires_at_ms"}"##,
        r##""expires_at_ms": {"type": ["integer", "null"], "minimum": 0, "description": "Absolute Unix milliseconds; needs format v5. Must be later than the server clock when the write is applied: an equal or earlier value is refused with 422 invalid_input and nothing is written"}, "ttl_ms": {"type": "integer", "minimum": 1, "description": "Lifetime in milliseconds, at least 1, counted from the moment the write is applied; needs format v5; mutually exclusive with expires_at_ms. 0 is refused with 422 invalid_input and nothing is written"}"##,
    ),
    // The collection batch route shares its handler with `/v1/batches`: it
    // answers 204, never 200, and the errors of that route plus a 403 (route
    // authorization and the collection guard) and the guard's 404. The
    // documented answers follow the handler.
    (
        r##""responses":{"200":{"description":"Synced atomic batch"},"429":"##,
        r##""responses":{"204":{"description":"Entire batch durably committed, or empty no-op"},"401":{"$ref":"#/components/responses/Unauthorized"},"400":{"$ref":"#/components/responses/BadRequest"},"403":{"description":"read_only or forbidden_scope: the token or credential may not write to this collection (operation, tenant or collection grant refused).","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"404":{"description":"collection_not_found: the collection does not exist or is not visible to this caller.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"409":{"$ref":"#/components/responses/Upgrade"},"413":{"$ref":"#/components/responses/TooLarge"},"415":{"$ref":"#/components/responses/MediaType"},"422":{"$ref":"#/components/responses/InvalidInput"},"503":{"$ref":"#/components/responses/Unavailable"},"429":"##,
    ),
];

/// Short reviewed wording changes on the already-approved KV route additions.
const APPROVED_OPENAPI_ADDITION_EDITS: &[(&str, &str)] = &[
    (
        "none is memory-only. Indeterminate outcomes must not be blindly retried.",
        "none is memory-only. Successful mutations are acknowledged after commit. Indeterminate outcomes must not be blindly retried.",
    ),
    (
        "Follows configured durability, not an unconditional sync.",
        "Follows configured durability, not an unconditional sync. Successful mutations are acknowledged after commit.",
    ),
    (
        "after is exclusive. One owner guard",
        "after is exclusive. Query strings use form decoding: + is a space and literal plus must be encoded as %2B. One owner guard",
    ),
];

/// The baseline capture with every `APPROVED_OPENAPI_EDITS` entry applied.
fn edited_golden() -> String {
    let mut golden = fixture("openapi.json");
    for (baseline, served) in APPROVED_OPENAPI_EDITS {
        assert_eq!(
            golden.matches(baseline).count(),
            1,
            "approved OpenAPI edit must match exactly one baseline location"
        );
        assert!(!baseline.contains('\n') && !served.contains('\n'));
        golden = golden.replacen(baseline, served, 1);
    }
    golden
}

/// Reviewed post-baseline OpenAPI additions, each matched byte-for-byte (LF)
/// at a line boundary; any other difference from the `347b01d` capture
/// fails REG-008. The `347b01d` capture already contains every change
/// published before it (the `x-durable-group-commit` block, the
/// `AutoMaintenancePolicy` retry fields and `MaintenanceStatus.rebuild.phase`).
/// Approved since: the tenant lifecycle routes (usage, settings, suspend,
/// resume), the dashboard account routes (`/v1/auth/*`, `/v1/admin/users*`)
/// and their `Tenant`, `TenantUsage`, `CapabilityIdentity`, `UserAccount`,
/// `AuthSession` and `AuthSignedIn` schemas; they are new whole lines and
/// leave existing lines untouched. v1.2.20 adds the graph-build-only
/// `graphs` and `graph_access` account fields to those three account lines.
/// Private Gap 8 REST step 2a adds exactly ProviderAlias, EmbedResponse and
/// seven named provider error schemas. Existing-route and schema changes are
/// reviewed separately in APPROVED_OPENAPI_EDITS; the baseline is not re-pinned.
/// Extend this list only for reviewed, intentional additions, or re-pin the
/// baseline with `python scripts\graph_baseline.py fixtures --rev <sha>`.
const APPROVED_OPENAPI_ADDITIONS: &[&str] = &[
    // Agent memory: one tag, nine POST paths and the frozen wire schemas.
    concat!(
        r##"  "tags":[{"name":"Memory","description":"Explicitly provisioned scoped agent memory"}],"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/memory/{binding}/remember":{"post":{"operationId":"MemoryRemember","tags":["Memory"],"description":"Remember entity and episodic items in one exact scope. The first slice supports entity and episodic items only; unsupported capabilities are refused, never approximated.","security":[{"BearerAuth":[]}],"parameters":[{"name":"binding","in":"path","required":true,"schema":{"$ref":"#/components/schemas/MemoryBinding"}}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryRememberRequest"}}}},"responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryMutationResponse"}}}},"400":{"description":"invalid_json: malformed JSON, duplicate keys or a second JSON value.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"401":{"description":"unauthorized: authentication required; no lookup or egress.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"403":{"description":"forbidden_scope, read_only, tenant_suspended or tenant_unavailable: refused before existence lookup; unknown and ungranted bindings are indistinguishable.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"404":{"description":"memory_not_found, receipt_not_found or erasure_not_found: authorized absent target.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"409":{"description":"memory_not_configured, memory_capability_unavailable, memory_feed_unsupported, idempotency_conflict, memory_slot_conflict, memory_attachment_conflict, memory_deletion_pending, memory_job_reconciliation_required or index_not_ready: binding/capability/feed/conflict/barrier refusal; no lazy index build.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"412":{"description":"precondition_failed: object, slot, input or job revision/generation mismatch; details.field identifies the guard.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"413":{"description":"invalid_json for an oversized REST request. response_too_large is a gRPC preflight or MCP/CLI response-bound error, not a REST response cap.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"415":{"description":"invalid_json: unsupported request media type (extractor status).","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"422":{"description":"invalid_input for a wrong type or shape, an unknown or missing field, or a semantic, arithmetic, frame or work bound (details.field names the field); memory_unsupported_ranking or memory_bounded_plan_unsupported without fallback.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"429":{"description":"rate_limited or quota_exceeded: configured credential rate or live-vector quota, not per-request work budget.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer","enum":[1]}}}},"500":{"description":"internal_error: sanitized unexpected failure or integrity fault.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"503":{"description":"busy, unavailable or poisoned; commit_indeterminate requires reopening a healthy database and reconciling the receipt, never blind retry.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"504":{"description":"deadline_exceeded: engine/adapter deadline before the commit point; no permitted partial answer.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}}}}},"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/memory/{binding}/recall":{"post":{"operationId":"MemoryRecall","tags":["Memory"],"description":"Recall retained scoped memories with explicit bounded work and ranking. Text does not implicitly invoke an embedding provider; no recall pagination is provided.","security":[{"BearerAuth":[]}],"parameters":[{"name":"binding","in":"path","required":true,"schema":{"$ref":"#/components/schemas/MemoryBinding"}}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryRecallRequest"}}}},"responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryRecallResponse"}}}},"400":{"description":"invalid_json: malformed JSON, duplicate keys or a second JSON value.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"401":{"description":"unauthorized: authentication required; no lookup or egress.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"403":{"description":"forbidden_scope, read_only, tenant_suspended or tenant_unavailable: refused before existence lookup; unknown and ungranted bindings are indistinguishable.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"404":{"description":"memory_not_found, receipt_not_found or erasure_not_found: authorized absent target.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"409":{"description":"memory_not_configured, memory_capability_unavailable, memory_feed_unsupported, idempotency_conflict, memory_slot_conflict, memory_attachment_conflict, memory_deletion_pending, memory_job_reconciliation_required or index_not_ready: binding/capability/feed/conflict/barrier refusal; no lazy index build.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"412":{"description":"precondition_failed: object, slot, input or job revision/generation mismatch; details.field identifies the guard.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"413":{"description":"invalid_json for an oversized REST request. response_too_large is a gRPC preflight or MCP/CLI response-bound error, not a REST response cap.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"415":{"description":"invalid_json: unsupported request media type (extractor status).","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"422":{"description":"invalid_input for a wrong type or shape, an unknown or missing field, or a semantic, arithmetic, frame or work bound (details.field names the field); memory_unsupported_ranking or memory_bounded_plan_unsupported without fallback.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"429":{"description":"rate_limited or quota_exceeded: configured credential rate or live-vector quota, not per-request work budget.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer","enum":[1]}}}},"500":{"description":"internal_error: sanitized unexpected failure or integrity fault.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"503":{"description":"busy, unavailable or poisoned; commit_indeterminate requires reopening a healthy database and reconciling the receipt, never blind retry.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"504":{"description":"deadline_exceeded: engine/adapter deadline before the commit point; no permitted partial answer.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}}}}},"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/memory/{binding}/facts/query":{"post":{"operationId":"MemoryFindFacts","tags":["Memory"],"description":"Query retained facts in one exact slot. The first slice refuses this operation with 409 memory_capability_unavailable; error.details.feature names the missing capability.","security":[{"BearerAuth":[]}],"parameters":[{"name":"binding","in":"path","required":true,"schema":{"$ref":"#/components/schemas/MemoryBinding"}}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryFactsRequest"}}}},"responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryFactsResponse"}}}},"400":{"description":"invalid_json: malformed JSON, duplicate keys or a second JSON value.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"401":{"description":"unauthorized: authentication required; no lookup or egress.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"403":{"description":"forbidden_scope, read_only, tenant_suspended or tenant_unavailable: refused before existence lookup; unknown and ungranted bindings are indistinguishable.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"404":{"description":"memory_not_found, receipt_not_found or erasure_not_found: authorized absent target.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"409":{"description":"memory_not_configured, memory_capability_unavailable, memory_feed_unsupported, idempotency_conflict, memory_slot_conflict, memory_attachment_conflict, memory_deletion_pending, memory_job_reconciliation_required or index_not_ready: binding/capability/feed/conflict/barrier refusal; no lazy index build.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"412":{"description":"precondition_failed: object, slot, input or job revision/generation mismatch; details.field identifies the guard.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"413":{"description":"invalid_json for an oversized REST request. response_too_large is a gRPC preflight or MCP/CLI response-bound error, not a REST response cap.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"415":{"description":"invalid_json: unsupported request media type (extractor status).","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"422":{"description":"invalid_input for a wrong type or shape, an unknown or missing field, or a semantic, arithmetic, frame or work bound (details.field names the field); memory_unsupported_ranking or memory_bounded_plan_unsupported without fallback.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"429":{"description":"rate_limited or quota_exceeded: configured credential rate or live-vector quota, not per-request work budget.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer","enum":[1]}}}},"500":{"description":"internal_error: sanitized unexpected failure or integrity fault.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"503":{"description":"busy, unavailable or poisoned; commit_indeterminate requires reopening a healthy database and reconciling the receipt, never blind retry.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"504":{"description":"deadline_exceeded: engine/adapter deadline before the commit point; no permitted partial answer.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}}}}},"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/memory/{binding}/facts/correct":{"post":{"operationId":"MemoryCorrectFacts","tags":["Memory"],"description":"Correct facts with exact series and slot revisions. The first slice refuses this operation with 409 memory_capability_unavailable; error.details.feature names the missing capability.","security":[{"BearerAuth":[]}],"parameters":[{"name":"binding","in":"path","required":true,"schema":{"$ref":"#/components/schemas/MemoryBinding"}}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryCorrectionRequest"}}}},"responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryMutationResponse"}}}},"400":{"description":"invalid_json: malformed JSON, duplicate keys or a second JSON value.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"401":{"description":"unauthorized: authentication required; no lookup or egress.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"403":{"description":"forbidden_scope, read_only, tenant_suspended or tenant_unavailable: refused before existence lookup; unknown and ungranted bindings are indistinguishable.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"404":{"description":"memory_not_found, receipt_not_found or erasure_not_found: authorized absent target.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"409":{"description":"memory_not_configured, memory_capability_unavailable, memory_feed_unsupported, idempotency_conflict, memory_slot_conflict, memory_attachment_conflict, memory_deletion_pending, memory_job_reconciliation_required or index_not_ready: binding/capability/feed/conflict/barrier refusal; no lazy index build.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"412":{"description":"precondition_failed: object, slot, input or job revision/generation mismatch; details.field identifies the guard.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"413":{"description":"invalid_json for an oversized REST request. response_too_large is a gRPC preflight or MCP/CLI response-bound error, not a REST response cap.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"415":{"description":"invalid_json: unsupported request media type (extractor status).","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"422":{"description":"invalid_input for a wrong type or shape, an unknown or missing field, or a semantic, arithmetic, frame or work bound (details.field names the field); memory_unsupported_ranking or memory_bounded_plan_unsupported without fallback.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"429":{"description":"rate_limited or quota_exceeded: configured credential rate or live-vector quota, not per-request work budget.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer","enum":[1]}}}},"500":{"description":"internal_error: sanitized unexpected failure or integrity fault.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"503":{"description":"busy, unavailable or poisoned; commit_indeterminate requires reopening a healthy database and reconciling the receipt, never blind retry.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"504":{"description":"deadline_exceeded: engine/adapter deadline before the commit point; no permitted partial answer.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}}}}},"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/memory/{binding}/consolidate":{"post":{"operationId":"MemoryConsolidate","tags":["Memory"],"description":"Consolidate caller-supplied episodic output from revision-checked inputs. The first slice supports C1: one retained source summary only; it does not delete the source.","security":[{"BearerAuth":[]}],"parameters":[{"name":"binding","in":"path","required":true,"schema":{"$ref":"#/components/schemas/MemoryBinding"}}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryConsolidateRequest"}}}},"responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"oneOf":[{"$ref":"#/components/schemas/MemoryMutationResponse"},{"$ref":"#/components/schemas/MemoryPlanResponse"}]}}}},"400":{"description":"invalid_json: malformed JSON, duplicate keys or a second JSON value.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"401":{"description":"unauthorized: authentication required; no lookup or egress.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"403":{"description":"forbidden_scope, read_only, tenant_suspended or tenant_unavailable: refused before existence lookup; unknown and ungranted bindings are indistinguishable.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"404":{"description":"memory_not_found, receipt_not_found or erasure_not_found: authorized absent target.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"409":{"description":"memory_not_configured, memory_capability_unavailable, memory_feed_unsupported, idempotency_conflict, memory_slot_conflict, memory_attachment_conflict, memory_deletion_pending, memory_job_reconciliation_required or index_not_ready: binding/capability/feed/conflict/barrier refusal; no lazy index build.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"412":{"description":"precondition_failed: object, slot, input or job revision/generation mismatch; details.field identifies the guard.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"413":{"description":"invalid_json for an oversized REST request. response_too_large is a gRPC preflight or MCP/CLI response-bound error, not a REST response cap.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"415":{"description":"invalid_json: unsupported request media type (extractor status).","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"422":{"description":"invalid_input for a wrong type or shape, an unknown or missing field, or a semantic, arithmetic, frame or work bound (details.field names the field); memory_unsupported_ranking or memory_bounded_plan_unsupported without fallback.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"429":{"description":"rate_limited or quota_exceeded: configured credential rate or live-vector quota, not per-request work budget.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer","enum":[1]}}}},"500":{"description":"internal_error: sanitized unexpected failure or integrity fault.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"503":{"description":"busy, unavailable or poisoned; commit_indeterminate requires reopening a healthy database and reconciling the receipt, never blind retry.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"504":{"description":"deadline_exceeded: engine/adapter deadline before the commit point; no permitted partial answer.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}}}}},"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/memory/{binding}/forget":{"post":{"operationId":"MemoryForget","tags":["Memory"],"description":"Forget exact targets using logical deletion or retention expiry; 202 denotes a pending job, never secure erasure. The first slice supports IDs with logical_delete only.","security":[{"BearerAuth":[]}],"parameters":[{"name":"binding","in":"path","required":true,"schema":{"$ref":"#/components/schemas/MemoryBinding"}}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryForgetRequest"}}}},"responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryMutationResponse"}}}},"202":{"description":"Pending logical deletion; not secure erasure","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryMutationResponse"}}}},"400":{"description":"invalid_json: malformed JSON, duplicate keys or a second JSON value.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"401":{"description":"unauthorized: authentication required; no lookup or egress.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"403":{"description":"forbidden_scope, read_only, tenant_suspended or tenant_unavailable: refused before existence lookup; unknown and ungranted bindings are indistinguishable.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"404":{"description":"memory_not_found, receipt_not_found or erasure_not_found: authorized absent target.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"409":{"description":"memory_not_configured, memory_capability_unavailable, memory_feed_unsupported, idempotency_conflict, memory_slot_conflict, memory_attachment_conflict, memory_deletion_pending, memory_job_reconciliation_required or index_not_ready: binding/capability/feed/conflict/barrier refusal; no lazy index build.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"412":{"description":"precondition_failed: object, slot, input or job revision/generation mismatch; details.field identifies the guard.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"413":{"description":"invalid_json for an oversized REST request. response_too_large is a gRPC preflight or MCP/CLI response-bound error, not a REST response cap.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"415":{"description":"invalid_json: unsupported request media type (extractor status).","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"422":{"description":"invalid_input for a wrong type or shape, an unknown or missing field, or a semantic, arithmetic, frame or work bound (details.field names the field); memory_unsupported_ranking or memory_bounded_plan_unsupported without fallback.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"429":{"description":"rate_limited or quota_exceeded: configured credential rate or live-vector quota, not per-request work budget.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer","enum":[1]}}}},"500":{"description":"internal_error: sanitized unexpected failure or integrity fault.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"503":{"description":"busy, unavailable or poisoned; commit_indeterminate requires reopening a healthy database and reconciling the receipt, never blind retry.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"504":{"description":"deadline_exceeded: engine/adapter deadline before the commit point; no permitted partial answer.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}}}}},"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/memory/{binding}/receipts/lookup":{"post":{"operationId":"MemoryGetReceipt","tags":["Memory"],"description":"Look up a retained mutation receipt in one exact scope. The first slice refuses remember receipt lookup with 409 memory_capability_unavailable and error.details.feature remember_receipt_lookup.","security":[{"BearerAuth":[]}],"parameters":[{"name":"binding","in":"path","required":true,"schema":{"$ref":"#/components/schemas/MemoryBinding"}}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryReceiptRequest"}}}},"responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryReceiptResponse"}}}},"400":{"description":"invalid_json: malformed JSON, duplicate keys or a second JSON value.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"401":{"description":"unauthorized: authentication required; no lookup or egress.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"403":{"description":"forbidden_scope, read_only, tenant_suspended or tenant_unavailable: refused before existence lookup; unknown and ungranted bindings are indistinguishable.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"404":{"description":"memory_not_found, receipt_not_found or erasure_not_found: authorized absent target.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"409":{"description":"memory_not_configured, memory_capability_unavailable, memory_feed_unsupported, idempotency_conflict, memory_slot_conflict, memory_attachment_conflict, memory_deletion_pending, memory_job_reconciliation_required or index_not_ready: binding/capability/feed/conflict/barrier refusal; no lazy index build.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"412":{"description":"precondition_failed: object, slot, input or job revision/generation mismatch; details.field identifies the guard.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"413":{"description":"invalid_json for an oversized REST request. response_too_large is a gRPC preflight or MCP/CLI response-bound error, not a REST response cap.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"415":{"description":"invalid_json: unsupported request media type (extractor status).","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"422":{"description":"invalid_input for a wrong type or shape, an unknown or missing field, or a semantic, arithmetic, frame or work bound (details.field names the field); memory_unsupported_ranking or memory_bounded_plan_unsupported without fallback.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"429":{"description":"rate_limited or quota_exceeded: configured credential rate or live-vector quota, not per-request work budget.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer","enum":[1]}}}},"500":{"description":"internal_error: sanitized unexpected failure or integrity fault.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"503":{"description":"busy, unavailable or poisoned; commit_indeterminate requires reopening a healthy database and reconciling the receipt, never blind retry.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"504":{"description":"deadline_exceeded: engine/adapter deadline before the commit point; no permitted partial answer.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}}}}},"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/memory/{binding}/erasures/get":{"post":{"operationId":"MemoryGetErasure","tags":["Memory"],"description":"Read erasure status without claiming secure overwrite or backup purge. The first slice refuses this operation with 409 memory_capability_unavailable; error.details.feature names the missing capability.","security":[{"BearerAuth":[]}],"parameters":[{"name":"binding","in":"path","required":true,"schema":{"$ref":"#/components/schemas/MemoryBinding"}}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryErasureRequest"}}}},"responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryErasureResponse"}}}},"400":{"description":"invalid_json: malformed JSON, duplicate keys or a second JSON value.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"401":{"description":"unauthorized: authentication required; no lookup or egress.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"403":{"description":"forbidden_scope, read_only, tenant_suspended or tenant_unavailable: refused before existence lookup; unknown and ungranted bindings are indistinguishable.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"404":{"description":"memory_not_found, receipt_not_found or erasure_not_found: authorized absent target.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"409":{"description":"memory_not_configured, memory_capability_unavailable, memory_feed_unsupported, idempotency_conflict, memory_slot_conflict, memory_attachment_conflict, memory_deletion_pending, memory_job_reconciliation_required or index_not_ready: binding/capability/feed/conflict/barrier refusal; no lazy index build.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"412":{"description":"precondition_failed: object, slot, input or job revision/generation mismatch; details.field identifies the guard.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"413":{"description":"invalid_json for an oversized REST request. response_too_large is a gRPC preflight or MCP/CLI response-bound error, not a REST response cap.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"415":{"description":"invalid_json: unsupported request media type (extractor status).","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"422":{"description":"invalid_input for a wrong type or shape, an unknown or missing field, or a semantic, arithmetic, frame or work bound (details.field names the field); memory_unsupported_ranking or memory_bounded_plan_unsupported without fallback.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"429":{"description":"rate_limited or quota_exceeded: configured credential rate or live-vector quota, not per-request work budget.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer","enum":[1]}}}},"500":{"description":"internal_error: sanitized unexpected failure or integrity fault.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"503":{"description":"busy, unavailable or poisoned; commit_indeterminate requires reopening a healthy database and reconciling the receipt, never blind retry.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"504":{"description":"deadline_exceeded: engine/adapter deadline before the commit point; no permitted partial answer.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}}}}},"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/memory/{binding}/erasures/resume":{"post":{"operationId":"MemoryResumeErasure","tags":["Memory"],"description":"Resume one bounded erasure chunk with exact revision, generation and token; 202 denotes pending work. The first slice refuses this operation with 409 memory_capability_unavailable; error.details.feature names the missing capability.","security":[{"BearerAuth":[]}],"parameters":[{"name":"binding","in":"path","required":true,"schema":{"$ref":"#/components/schemas/MemoryBinding"}}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryResumeRequest"}}}},"responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryMutationResponse"}}}},"202":{"description":"Pending logical deletion; not secure erasure","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryMutationResponse"}}}},"400":{"description":"invalid_json: malformed JSON, duplicate keys or a second JSON value.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"401":{"description":"unauthorized: authentication required; no lookup or egress.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"403":{"description":"forbidden_scope, read_only, tenant_suspended or tenant_unavailable: refused before existence lookup; unknown and ungranted bindings are indistinguishable.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"404":{"description":"memory_not_found, receipt_not_found or erasure_not_found: authorized absent target.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"409":{"description":"memory_not_configured, memory_capability_unavailable, memory_feed_unsupported, idempotency_conflict, memory_slot_conflict, memory_attachment_conflict, memory_deletion_pending, memory_job_reconciliation_required or index_not_ready: binding/capability/feed/conflict/barrier refusal; no lazy index build.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"412":{"description":"precondition_failed: object, slot, input or job revision/generation mismatch; details.field identifies the guard.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"413":{"description":"invalid_json for an oversized REST request. response_too_large is a gRPC preflight or MCP/CLI response-bound error, not a REST response cap.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"415":{"description":"invalid_json: unsupported request media type (extractor status).","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"422":{"description":"invalid_input for a wrong type or shape, an unknown or missing field, or a semantic, arithmetic, frame or work bound (details.field names the field); memory_unsupported_ranking or memory_bounded_plan_unsupported without fallback.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"429":{"description":"rate_limited or quota_exceeded: configured credential rate or live-vector quota, not per-request work budget.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer","enum":[1]}}}},"500":{"description":"internal_error: sanitized unexpected failure or integrity fault.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"503":{"description":"busy, unavailable or poisoned; commit_indeterminate requires reopening a healthy database and reconciling the receipt, never blind retry.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}},"504":{"description":"deadline_exceeded: engine/adapter deadline before the commit point; no permitted partial answer.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/MemoryError"}}}}}}},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryBinding":{"type":"string","pattern":"^[a-z][a-z0-9_-]{0,62}$"},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryKey":{"type":"string","minLength":1,"maxLength":256},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryU64":{"type":"string","pattern":"^(0|[1-9][0-9]*)$"},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryI64":{"type":"string","pattern":"^(0|-?[1-9][0-9]*)$"},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryKind":{"type":"string","enum":["entity","episodic","fact","relation"]},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryOperation":{"type":"string","enum":["remember","correct_facts","consolidate","forget","resume_erasure"]},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryScope":{"type":"object","required":["agent"],"properties":{"agent":{"$ref":"#/components/schemas/MemoryKey"},"user":{"$ref":"#/components/schemas/MemoryKey"},"session":{"$ref":"#/components/schemas/MemoryKey"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryEffectiveScope":{"type":"object","required":["tenant","agent"],"properties":{"tenant":{"$ref":"#/components/schemas/MemoryKey"},"agent":{"$ref":"#/components/schemas/MemoryKey"},"user":{"$ref":"#/components/schemas/MemoryKey"},"session":{"$ref":"#/components/schemas/MemoryKey"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryExpected":{"oneOf":[{"type":"object","required":["absent"],"properties":{"absent":{"type":"boolean","enum":[true]}},"additionalProperties":false},{"type":"object","required":["revision"],"properties":{"revision":{"$ref":"#/components/schemas/MemoryU64"}},"additionalProperties":false}]},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryInterval":{"type":"object","required":["valid_from_ms","valid_to_ms"],"properties":{"valid_from_ms":{"anyOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"null"}]},"valid_to_ms":{"anyOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"null"}]}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryRef":{"type":"object","required":["id","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"$ref":"#/components/schemas/MemoryU64"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemorySource":{"type":"object","required":["source_id","source_kind"],"properties":{"source_id":{"$ref":"#/components/schemas/MemoryKey"},"source_kind":{"$ref":"#/components/schemas/MemoryKey"},"uri":{"type":"string","maxLength":256},"transformation_id":{"$ref":"#/components/schemas/MemoryKey"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryProvenance":{"type":"object","required":["source_id","source_kind","content_digest"],"properties":{"source_id":{"$ref":"#/components/schemas/MemoryKey"},"source_kind":{"$ref":"#/components/schemas/MemoryKey"},"uri":{"type":"string","maxLength":256},"transformation_id":{"$ref":"#/components/schemas/MemoryKey"},"content_digest":{"type":"string","pattern":"^v1:[0-9a-f]{64}$"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryScalar":{"oneOf":[{"type":"object","required":["type","value"],"properties":{"type":{"enum":["string"],"type":"string"},"value":{"type":"string","maxLength":4096}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["integer"],"type":"string"},"value":{"$ref":"#/components/schemas/MemoryI64"}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["float"],"type":"string"},"value":{"type":"number"}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["boolean"],"type":"string"},"value":{"type":"boolean"}},"additionalProperties":false}]},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryProperties":{"type":"object","maxProperties":64,"additionalProperties":{"$ref":"#/components/schemas/MemoryScalar"}},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryCondition":{"type":"object","required":["field","op"],"properties":{"field":{"$ref":"#/components/schemas/MemoryKey"},"op":{"enum":["eq","lt","lte","gt","gte","exists"],"type":"string"},"value":{"$ref":"#/components/schemas/MemoryScalar"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryFilter":{"type":"object","properties":{"all":{"type":"array","items":{"$ref":"#/components/schemas/MemoryCondition"},"maxItems":64},"any":{"type":"array","items":{"$ref":"#/components/schemas/MemoryCondition"},"maxItems":64}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryEvidenceRef":{"type":"object","required":["collection","record"],"properties":{"collection":{"$ref":"#/components/schemas/MemoryBinding"},"record":{"$ref":"#/components/schemas/MemoryU64"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryEvidence":{"type":"object","required":["collection","record"],"properties":{"collection":{"$ref":"#/components/schemas/MemoryBinding"},"record":{"$ref":"#/components/schemas/MemoryU64"},"vector":{"type":"array","items":{"type":"number"},"minItems":1},"text":{"type":"string","maxLength":4096},"payload_base64":{"type":"string","format":"byte"},"metadata":{"$ref":"#/components/schemas/MemoryProperties"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryAssertion":{"type":"object","required":["subject_id","predicate","slot_key","destination_id","interval","slot_expected"],"properties":{"subject_id":{"$ref":"#/components/schemas/MemoryKey"},"predicate":{"$ref":"#/components/schemas/MemoryKey"},"slot_key":{"$ref":"#/components/schemas/MemoryKey"},"destination_id":{"$ref":"#/components/schemas/MemoryKey"},"interval":{"$ref":"#/components/schemas/MemoryInterval"},"slot_expected":{"$ref":"#/components/schemas/MemoryExpected"},"disagreement":{"enum":["reject","disputed"],"default":"reject","type":"string"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryItem":{"type":"object","required":["id","kind","content","observed_at_ms","source"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"kind":{"$ref":"#/components/schemas/MemoryKind"},"content":{"type":"string","maxLength":4096},"observed_at_ms":{"$ref":"#/components/schemas/MemoryI64"},"importance":{"type":"number","minimum":0,"maximum":1,"default":0.5},"confidence":{"type":"number","minimum":0,"maximum":1},"expires_at_ms":{"$ref":"#/components/schemas/MemoryU64"},"source":{"$ref":"#/components/schemas/MemorySource"},"provenance":{"type":"array","items":{"$ref":"#/components/schemas/MemorySource"},"maxItems":64},"references":{"type":"array","items":{"$ref":"#/components/schemas/MemoryRef"},"maxItems":256},"evidence":{"type":"array","items":{"$ref":"#/components/schemas/MemoryEvidenceRef"},"maxItems":64},"metadata":{"$ref":"#/components/schemas/MemoryProperties"},"assertion":{"$ref":"#/components/schemas/MemoryAssertion"},"expected":{"$ref":"#/components/schemas/MemoryExpected"},"merge":{"enum":["none","merge_provenance"],"default":"none","type":"string"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryMutation":{"type":"object","required":["scope","idempotency_key"],"properties":{"scope":{"$ref":"#/components/schemas/MemoryScope"},"idempotency_key":{"$ref":"#/components/schemas/MemoryKey"},"max_references":{"type":"integer","minimum":1,"maximum":256,"default":64},"deadline_ms":{"type":"integer","minimum":1,"maximum":5000,"default":250},"require_complete_feed":{"type":"boolean","default":false}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryRememberRequest":{"type":"object","required":["scope","idempotency_key","items"],"properties":{"scope":{"$ref":"#/components/schemas/MemoryScope"},"idempotency_key":{"$ref":"#/components/schemas/MemoryKey"},"max_references":{"type":"integer","minimum":1,"maximum":256,"default":64},"deadline_ms":{"type":"integer","minimum":1,"maximum":5000,"default":250},"require_complete_feed":{"type":"boolean","default":false},"items":{"type":"array","items":{"type":"object","required":["id","kind","content","observed_at_ms","source"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"kind":{"$ref":"#/components/schemas/MemoryKind"},"content":{"type":"string","maxLength":4096},"observed_at_ms":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},"importance":{"type":"number","minimum":0,"maximum":1,"default":0.5},"confidence":{"type":"number","minimum":0,"maximum":1},"expires_at_ms":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]},"source":{"$ref":"#/components/schemas/MemorySource"},"provenance":{"type":"array","items":{"$ref":"#/components/schemas/MemorySource"},"maxItems":64},"references":{"type":"array","items":{"type":"object","required":["id","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false},"maxItems":256},"evidence":{"type":"array","items":{"type":"object","required":["collection","record"],"properties":{"collection":{"$ref":"#/components/schemas/MemoryBinding"},"record":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false},"maxItems":64},"metadata":{"type":"object","maxProperties":64,"additionalProperties":{"oneOf":[{"type":"object","required":["type","value"],"properties":{"type":{"enum":["string"],"type":"string"},"value":{"type":"string","maxLength":4096}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["integer"],"type":"string"},"value":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["float"],"type":"string"},"value":{"type":"number"}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["boolean"],"type":"string"},"value":{"type":"boolean"}},"additionalProperties":false}]}},"assertion":{"type":"object","required":["subject_id","predicate","slot_key","destination_id","interval","slot_expected"],"properties":{"subject_id":{"$ref":"#/components/schemas/MemoryKey"},"predicate":{"$ref":"#/components/schemas/MemoryKey"},"slot_key":{"$ref":"#/components/schemas/MemoryKey"},"destination_id":{"$ref":"#/components/schemas/MemoryKey"},"interval":{"type":"object","required":["valid_from_ms","valid_to_ms"],"properties":{"valid_from_ms":{"anyOf":[{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},{"type":"null"}]},"valid_to_ms":{"anyOf":[{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},{"type":"null"}]}},"additionalProperties":false},"slot_expected":{"oneOf":[{"type":"object","required":["absent"],"properties":{"absent":{"type":"boolean","enum":[true]}},"additionalProperties":false},{"type":"object","required":["revision"],"properties":{"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false}]},"disagreement":{"enum":["reject","disputed"],"default":"reject","type":"string"}},"additionalProperties":false},"expected":{"oneOf":[{"type":"object","required":["absent"],"properties":{"absent":{"type":"boolean","enum":[true]}},"additionalProperties":false},{"type":"object","required":["revision"],"properties":{"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false}]},"merge":{"enum":["none","merge_provenance"],"default":"none","type":"string"}},"additionalProperties":false},"minItems":1,"maxItems":4096},"evidence":{"type":"array","items":{"type":"object","required":["collection","record"],"properties":{"collection":{"$ref":"#/components/schemas/MemoryBinding"},"record":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]},"vector":{"type":"array","items":{"type":"number"},"minItems":1},"text":{"type":"string","maxLength":4096},"payload_base64":{"type":"string","format":"byte"},"metadata":{"type":"object","maxProperties":64,"additionalProperties":{"oneOf":[{"type":"object","required":["type","value"],"properties":{"type":{"enum":["string"],"type":"string"},"value":{"type":"string","maxLength":4096}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["integer"],"type":"string"},"value":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["float"],"type":"string"},"value":{"type":"number"}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["boolean"],"type":"string"},"value":{"type":"boolean"}},"additionalProperties":false}]}}},"additionalProperties":false},"maxItems":256}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryRanking":{"type":"object","properties":{"selection":{"enum":["weighted","exact","mmr"],"default":"weighted","type":"string"},"weights":{"type":"array","items":{"type":"number","minimum":0},"minItems":3,"maxItems":3,"default":[1,0,0]},"half_life_ms":{"type":"number","exclusiveMinimum":0},"future_observations":{"enum":["reject","clamp"],"default":"reject","type":"string"},"lambda":{"type":"number","minimum":0,"maximum":1,"default":0.5}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryLimits":{"type":"object","properties":{"candidates":{"type":"integer","minimum":1,"maximum":1000,"default":200},"vector_components":{"type":"integer","minimum":1,"maximum":4000000,"default":1000000},"evidence_bytes":{"type":"integer","minimum":1,"maximum":4194304,"default":1048576},"mmr_components":{"type":"integer","minimum":1,"maximum":100000000,"default":10000000},"deadline_ms":{"type":"integer","minimum":1,"maximum":5000,"default":250},"max_hops":{"type":"integer","minimum":0,"maximum":4,"default":2},"max_visited_nodes":{"type":"integer","minimum":1,"maximum":10000,"default":1000},"max_edges_examined":{"type":"integer","minimum":1,"maximum":100000,"default":10000},"max_edges_per_node":{"type":"integer","minimum":1,"maximum":10000,"default":1000},"max_output_nodes":{"type":"integer","minimum":0,"maximum":1000,"default":100},"max_output_edges":{"type":"integer","minimum":0,"maximum":2000,"default":200},"max_output_paths":{"type":"integer","minimum":0,"maximum":100,"default":20}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryProjection":{"type":"object","properties":{"snippets":{"type":"boolean","default":true},"provenance":{"type":"boolean","default":true},"paths":{"type":"boolean","default":false},"metadata":{"type":"array","items":{"$ref":"#/components/schemas/MemoryKey"},"maxItems":64,"default":[]}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryRecallRequest":{"type":"object","required":["scope"],"properties":{"scope":{"$ref":"#/components/schemas/MemoryScope"},"scopes":{"type":"array","items":{"$ref":"#/components/schemas/MemoryScope"},"minItems":1,"maxItems":16},"vector":{"type":"array","items":{"type":"number"},"minItems":1},"text":{"type":"string","minLength":1,"maxLength":16384},"embedding_provider":{"$ref":"#/components/schemas/MemoryKey"},"entity_seeds":{"type":"array","items":{"$ref":"#/components/schemas/MemoryKey"},"maxItems":64},"kinds":{"type":"array","items":{"$ref":"#/components/schemas/MemoryKind"},"maxItems":4},"filter":{"type":"object","properties":{"all":{"type":"array","items":{"type":"object","required":["field","op"],"properties":{"field":{"$ref":"#/components/schemas/MemoryKey"},"op":{"enum":["eq","lt","lte","gt","gte","exists"],"type":"string"},"value":{"oneOf":[{"type":"object","required":["type","value"],"properties":{"type":{"enum":["string"],"type":"string"},"value":{"type":"string","maxLength":4096}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["integer"],"type":"string"},"value":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["float"],"type":"string"},"value":{"type":"number"}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["boolean"],"type":"string"},"value":{"type":"boolean"}},"additionalProperties":false}]}},"additionalProperties":false},"maxItems":64},"any":{"type":"array","items":{"type":"object","required":["field","op"],"properties":{"field":{"$ref":"#/components/schemas/MemoryKey"},"op":{"enum":["eq","lt","lte","gt","gte","exists"],"type":"string"},"value":{"oneOf":[{"type":"object","required":["type","value"],"properties":{"type":{"enum":["string"],"type":"string"},"value":{"type":"string","maxLength":4096}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["integer"],"type":"string"},"value":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["float"],"type":"string"},"value":{"type":"number"}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["boolean"],"type":"string"},"value":{"type":"boolean"}},"additionalProperties":false}]}},"additionalProperties":false},"maxItems":64}},"additionalProperties":false},"as_of_ms":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},"ranking":{"$ref":"#/components/schemas/MemoryRanking"},"limits":{"$ref":"#/components/schemas/MemoryLimits"},"projection":{"$ref":"#/components/schemas/MemoryProjection"},"mode":{"enum":["exact","approximate"],"default":"exact","type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100,"default":20},"allow_partial":{"type":"boolean","default":false}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemorySlot":{"type":"object","required":["subject_id","predicate","slot_key"],"properties":{"subject_id":{"$ref":"#/components/schemas/MemoryKey"},"predicate":{"$ref":"#/components/schemas/MemoryKey"},"slot_key":{"$ref":"#/components/schemas/MemoryKey"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryFactsRequest":{"type":"object","required":["scope","slot"],"properties":{"scope":{"$ref":"#/components/schemas/MemoryScope"},"slot":{"$ref":"#/components/schemas/MemorySlot"},"as_of_ms":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},"history":{"type":"boolean","default":false},"limit":{"type":"integer","minimum":1,"maximum":100,"default":20},"limits":{"$ref":"#/components/schemas/MemoryLimits"},"projection":{"$ref":"#/components/schemas/MemoryProjection"},"allow_partial":{"type":"boolean","default":false}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryCorrectionRequest":{"type":"object","required":["scope","idempotency_key","slot","series","slot_revision","interval","action"],"properties":{"scope":{"$ref":"#/components/schemas/MemoryScope"},"idempotency_key":{"$ref":"#/components/schemas/MemoryKey"},"max_references":{"type":"integer","minimum":1,"maximum":256,"default":64},"deadline_ms":{"type":"integer","minimum":1,"maximum":5000,"default":250},"require_complete_feed":{"type":"boolean","default":false},"slot":{"$ref":"#/components/schemas/MemorySlot"},"series":{"type":"object","required":["id","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false},"slot_revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]},"interval":{"type":"object","required":["valid_from_ms","valid_to_ms"],"properties":{"valid_from_ms":{"anyOf":[{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},{"type":"null"}]},"valid_to_ms":{"anyOf":[{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},{"type":"null"}]}},"additionalProperties":false},"action":{"enum":["close","replace","remove"],"type":"string"},"at_ms":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},"correction_interval":{"type":"object","required":["valid_from_ms","valid_to_ms"],"properties":{"valid_from_ms":{"anyOf":[{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},{"type":"null"}]},"valid_to_ms":{"anyOf":[{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},{"type":"null"}]}},"additionalProperties":false},"replacement":{"type":"object","required":["id","kind","content","observed_at_ms","source"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"kind":{"$ref":"#/components/schemas/MemoryKind"},"content":{"type":"string","maxLength":4096},"observed_at_ms":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},"importance":{"type":"number","minimum":0,"maximum":1,"default":0.5},"confidence":{"type":"number","minimum":0,"maximum":1},"expires_at_ms":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]},"source":{"$ref":"#/components/schemas/MemorySource"},"provenance":{"type":"array","items":{"$ref":"#/components/schemas/MemorySource"},"maxItems":64},"references":{"type":"array","items":{"type":"object","required":["id","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false},"maxItems":256},"evidence":{"type":"array","items":{"type":"object","required":["collection","record"],"properties":{"collection":{"$ref":"#/components/schemas/MemoryBinding"},"record":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false},"maxItems":64},"metadata":{"type":"object","maxProperties":64,"additionalProperties":{"oneOf":[{"type":"object","required":["type","value"],"properties":{"type":{"enum":["string"],"type":"string"},"value":{"type":"string","maxLength":4096}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["integer"],"type":"string"},"value":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["float"],"type":"string"},"value":{"type":"number"}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["boolean"],"type":"string"},"value":{"type":"boolean"}},"additionalProperties":false}]}},"assertion":{"type":"object","required":["subject_id","predicate","slot_key","destination_id","interval","slot_expected"],"properties":{"subject_id":{"$ref":"#/components/schemas/MemoryKey"},"predicate":{"$ref":"#/components/schemas/MemoryKey"},"slot_key":{"$ref":"#/components/schemas/MemoryKey"},"destination_id":{"$ref":"#/components/schemas/MemoryKey"},"interval":{"type":"object","required":["valid_from_ms","valid_to_ms"],"properties":{"valid_from_ms":{"anyOf":[{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},{"type":"null"}]},"valid_to_ms":{"anyOf":[{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},{"type":"null"}]}},"additionalProperties":false},"slot_expected":{"oneOf":[{"type":"object","required":["absent"],"properties":{"absent":{"type":"boolean","enum":[true]}},"additionalProperties":false},{"type":"object","required":["revision"],"properties":{"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false}]},"disagreement":{"enum":["reject","disputed"],"default":"reject","type":"string"}},"additionalProperties":false},"expected":{"oneOf":[{"type":"object","required":["absent"],"properties":{"absent":{"type":"boolean","enum":[true]}},"additionalProperties":false},{"type":"object","required":["revision"],"properties":{"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false}]},"merge":{"enum":["none","merge_provenance"],"default":"none","type":"string"}},"additionalProperties":false},"evidence":{"type":"array","items":{"type":"object","required":["collection","record"],"properties":{"collection":{"$ref":"#/components/schemas/MemoryBinding"},"record":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]},"vector":{"type":"array","items":{"type":"number"},"minItems":1},"text":{"type":"string","maxLength":4096},"payload_base64":{"type":"string","format":"byte"},"metadata":{"type":"object","maxProperties":64,"additionalProperties":{"oneOf":[{"type":"object","required":["type","value"],"properties":{"type":{"enum":["string"],"type":"string"},"value":{"type":"string","maxLength":4096}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["integer"],"type":"string"},"value":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["float"],"type":"string"},"value":{"type":"number"}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["boolean"],"type":"string"},"value":{"type":"boolean"}},"additionalProperties":false}]}}},"additionalProperties":false},"maxItems":256}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryInput":{"type":"object","required":["id","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"$ref":"#/components/schemas/MemoryU64"},"disposition":{"enum":["retain","logical_delete"],"default":"retain","type":"string"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryConsolidateRequest":{"type":"object","required":["scope","idempotency_key","inputs","output"],"properties":{"scope":{"$ref":"#/components/schemas/MemoryScope"},"idempotency_key":{"$ref":"#/components/schemas/MemoryKey"},"max_references":{"type":"integer","minimum":1,"maximum":256,"default":64},"deadline_ms":{"type":"integer","minimum":1,"maximum":5000,"default":250},"require_complete_feed":{"type":"boolean","default":false},"inputs":{"type":"array","items":{"type":"object","required":["id","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]},"disposition":{"enum":["retain","logical_delete"],"default":"retain","type":"string"}},"additionalProperties":false},"minItems":1,"maxItems":256},"output":{"type":"object","required":["id","kind","content","observed_at_ms","source"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"kind":{"$ref":"#/components/schemas/MemoryKind"},"content":{"type":"string","maxLength":4096},"observed_at_ms":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},"importance":{"type":"number","minimum":0,"maximum":1,"default":0.5},"confidence":{"type":"number","minimum":0,"maximum":1},"expires_at_ms":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]},"source":{"$ref":"#/components/schemas/MemorySource"},"provenance":{"type":"array","items":{"$ref":"#/components/schemas/MemorySource"},"maxItems":64},"references":{"type":"array","items":{"type":"object","required":["id","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false},"maxItems":256},"evidence":{"type":"array","items":{"type":"object","required":["collection","record"],"properties":{"collection":{"$ref":"#/components/schemas/MemoryBinding"},"record":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false},"maxItems":64},"metadata":{"type":"object","maxProperties":64,"additionalProperties":{"oneOf":[{"type":"object","required":["type","value"],"properties":{"type":{"enum":["string"],"type":"string"},"value":{"type":"string","maxLength":4096}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["integer"],"type":"string"},"value":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["float"],"type":"string"},"value":{"type":"number"}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["boolean"],"type":"string"},"value":{"type":"boolean"}},"additionalProperties":false}]}},"assertion":{"type":"object","required":["subject_id","predicate","slot_key","destination_id","interval","slot_expected"],"properties":{"subject_id":{"$ref":"#/components/schemas/MemoryKey"},"predicate":{"$ref":"#/components/schemas/MemoryKey"},"slot_key":{"$ref":"#/components/schemas/MemoryKey"},"destination_id":{"$ref":"#/components/schemas/MemoryKey"},"interval":{"type":"object","required":["valid_from_ms","valid_to_ms"],"properties":{"valid_from_ms":{"anyOf":[{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},{"type":"null"}]},"valid_to_ms":{"anyOf":[{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]},{"type":"null"}]}},"additionalProperties":false},"slot_expected":{"oneOf":[{"type":"object","required":["absent"],"properties":{"absent":{"type":"boolean","enum":[true]}},"additionalProperties":false},{"type":"object","required":["revision"],"properties":{"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false}]},"disagreement":{"enum":["reject","disputed"],"default":"reject","type":"string"}},"additionalProperties":false},"expected":{"oneOf":[{"type":"object","required":["absent"],"properties":{"absent":{"type":"boolean","enum":[true]}},"additionalProperties":false},{"type":"object","required":["revision"],"properties":{"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false}]},"merge":{"enum":["none","merge_provenance"],"default":"none","type":"string"}},"additionalProperties":false},"evidence":{"type":"array","items":{"type":"object","required":["collection","record"],"properties":{"collection":{"$ref":"#/components/schemas/MemoryBinding"},"record":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]},"vector":{"type":"array","items":{"type":"number"},"minItems":1},"text":{"type":"string","maxLength":4096},"payload_base64":{"type":"string","format":"byte"},"metadata":{"type":"object","maxProperties":64,"additionalProperties":{"oneOf":[{"type":"object","required":["type","value"],"properties":{"type":{"enum":["string"],"type":"string"},"value":{"type":"string","maxLength":4096}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["integer"],"type":"string"},"value":{"oneOf":[{"$ref":"#/components/schemas/MemoryI64"},{"type":"integer","minimum":-9223372036854775808,"maximum":9223372036854775807}]}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["float"],"type":"string"},"value":{"type":"number"}},"additionalProperties":false},{"type":"object","required":["type","value"],"properties":{"type":{"enum":["boolean"],"type":"string"},"value":{"type":"boolean"}},"additionalProperties":false}]}}},"additionalProperties":false},"maxItems":256},"dry_run":{"type":"boolean","default":false}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryTarget":{"oneOf":[{"type":"object","required":["ids"],"properties":{"ids":{"type":"array","items":{"$ref":"#/components/schemas/MemoryRef"},"minItems":1,"maxItems":4096}},"additionalProperties":false},{"type":"object","required":["series"],"properties":{"series":{"$ref":"#/components/schemas/MemoryRef"}},"additionalProperties":false},{"type":"object","required":["entity"],"properties":{"entity":{"$ref":"#/components/schemas/MemoryRef"}},"additionalProperties":false},{"type":"object","required":["session"],"properties":{"session":{"type":"boolean","enum":[true]}},"additionalProperties":false},{"type":"object","required":["scope"],"properties":{"scope":{"type":"boolean","enum":[true]}},"additionalProperties":false}]},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryForgetRequest":{"type":"object","required":["scope","idempotency_key","target","mode"],"properties":{"scope":{"$ref":"#/components/schemas/MemoryScope"},"idempotency_key":{"$ref":"#/components/schemas/MemoryKey"},"max_references":{"type":"integer","minimum":1,"maximum":256,"default":64},"deadline_ms":{"type":"integer","minimum":1,"maximum":5000,"default":250},"require_complete_feed":{"type":"boolean","default":false},"target":{"oneOf":[{"type":"object","required":["ids"],"properties":{"ids":{"type":"array","items":{"type":"object","required":["id","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false},"minItems":1,"maxItems":4096}},"additionalProperties":false},{"type":"object","required":["series"],"properties":{"series":{"type":"object","required":["id","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false}},"additionalProperties":false},{"type":"object","required":["entity"],"properties":{"entity":{"type":"object","required":["id","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]}},"additionalProperties":false}},"additionalProperties":false},{"type":"object","required":["session"],"properties":{"session":{"type":"boolean","enum":[true]}},"additionalProperties":false},{"type":"object","required":["scope"],"properties":{"scope":{"type":"boolean","enum":[true]}},"additionalProperties":false}]},"mode":{"enum":["logical_delete","expire"],"type":"string"},"expires_at_ms":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]},"allow_job":{"type":"boolean","default":false}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryReceiptRequest":{"type":"object","required":["scope","operation","idempotency_key"],"properties":{"scope":{"$ref":"#/components/schemas/MemoryScope"},"operation":{"$ref":"#/components/schemas/MemoryOperation"},"idempotency_key":{"$ref":"#/components/schemas/MemoryKey"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryErasureRequest":{"type":"object","required":["scope","job_id"],"properties":{"scope":{"$ref":"#/components/schemas/MemoryScope"},"job_id":{"$ref":"#/components/schemas/MemoryKey"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryResumeRequest":{"type":"object","required":["scope","idempotency_key","job_id","revision","generation","resume_token"],"properties":{"scope":{"$ref":"#/components/schemas/MemoryScope"},"idempotency_key":{"$ref":"#/components/schemas/MemoryKey"},"max_references":{"type":"integer","minimum":1,"maximum":256,"default":64},"deadline_ms":{"type":"integer","minimum":1,"maximum":5000,"default":250},"require_complete_feed":{"type":"boolean","default":false},"job_id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]},"generation":{"oneOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"integer","minimum":0,"maximum":18446744073709551615}]},"resume_token":{"type":"string","minLength":1,"maxLength":4096}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryReceipt":{"type":"object","required":["id","operation","scope","sequence","digest_version"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"operation":{"$ref":"#/components/schemas/MemoryOperation"},"scope":{"$ref":"#/components/schemas/MemoryEffectiveScope"},"sequence":{"$ref":"#/components/schemas/MemoryU64"},"retry_until_ms":{"$ref":"#/components/schemas/MemoryU64"},"digest_version":{"enum":["memory-request-v1"],"type":"string"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryOutcome":{"type":"object","required":["id","action","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"action":{"enum":["created","merged","noop","corrected","deleted","expired","retained_shared"],"type":"string"},"revision":{"$ref":"#/components/schemas/MemoryU64"},"expires_at_ms":{"$ref":"#/components/schemas/MemoryU64"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryFeed":{"type":"object","required":["mode","temporal_complete","ttl_complete","consumer_purge_acknowledged"],"properties":{"mode":{"enum":["ordinary_changes"],"type":"string"},"temporal_complete":{"enum":[false],"type":"boolean"},"ttl_complete":{"enum":[false],"type":"boolean"},"consumer_purge_acknowledged":{"enum":[false],"type":"boolean"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryErasureNotice":{"type":"object","required":["live_state_only","secure_erasure","external_copies","restore_requires_manifest","backup_history_purge"],"properties":{"live_state_only":{"enum":[true],"type":"boolean"},"secure_erasure":{"enum":[false],"type":"boolean"},"external_copies":{"enum":["operator_managed"],"type":"string"},"restore_requires_manifest":{"enum":[true],"type":"boolean"},"backup_history_purge":{"enum":["not_verified"],"type":"string"},"manifest_id":{"$ref":"#/components/schemas/MemoryKey"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryErasure":{"type":"object","required":["id","scope","target","revision","generation","state","logical_visibility","deleted_count","remaining_count"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"scope":{"$ref":"#/components/schemas/MemoryEffectiveScope"},"target":{"$ref":"#/components/schemas/MemoryTarget"},"revision":{"$ref":"#/components/schemas/MemoryU64"},"generation":{"$ref":"#/components/schemas/MemoryU64"},"state":{"enum":["pending","logical"],"type":"string"},"logical_visibility":{"enum":["hidden"],"type":"string"},"deleted_count":{"$ref":"#/components/schemas/MemoryU64"},"remaining_count":{"anyOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"null"}]},"resume_token":{"type":"string","maxLength":4096}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryMutationResponse":{"type":"object","required":["receipt","replayed","outcomes","feed","remaining_work"],"properties":{"receipt":{"$ref":"#/components/schemas/MemoryReceipt"},"replayed":{"type":"boolean"},"outcomes":{"type":"array","items":{"$ref":"#/components/schemas/MemoryOutcome"},"maxItems":4096},"feed":{"$ref":"#/components/schemas/MemoryFeed"},"remaining_work":{"anyOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"null"}]},"state":{"enum":["logical","pending","retention_set"],"type":"string"},"erasure":{"$ref":"#/components/schemas/MemoryErasure"},"erasure_notice":{"$ref":"#/components/schemas/MemoryErasureNotice"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryReceiptResponse":{"type":"object","required":["result"],"properties":{"result":{"$ref":"#/components/schemas/MemoryMutationResponse"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryErasureResponse":{"type":"object","required":["erasure","erasure_notice","feed"],"properties":{"erasure":{"$ref":"#/components/schemas/MemoryErasure"},"erasure_notice":{"$ref":"#/components/schemas/MemoryErasureNotice"},"feed":{"$ref":"#/components/schemas/MemoryFeed"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryPlanResponse":{"type":"object","required":["dry_run","user_operations","encoded_bytes","conflicts","remaining_work"],"properties":{"dry_run":{"enum":[true],"type":"boolean"},"user_operations":{"$ref":"#/components/schemas/MemoryU64"},"encoded_bytes":{"$ref":"#/components/schemas/MemoryU64"},"conflicts":{"type":"array","items":{"$ref":"#/components/schemas/MemoryErrorDetail"},"maxItems":256},"remaining_work":{"anyOf":[{"$ref":"#/components/schemas/MemoryU64"},{"type":"null"}]}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryEvidenceView":{"type":"object","required":["collection","record"],"properties":{"collection":{"$ref":"#/components/schemas/MemoryBinding"},"record":{"$ref":"#/components/schemas/MemoryU64"},"snippet":{"type":"string","maxLength":4096}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryPath":{"type":"object","required":["nodes","edges"],"properties":{"nodes":{"type":"array","items":{"$ref":"#/components/schemas/MemoryU64"},"maxItems":5},"edges":{"type":"array","items":{"$ref":"#/components/schemas/MemoryU64"},"maxItems":4}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryScores":{"type":"object","required":["R","D","I","S"],"properties":{"R":{"type":"number"},"D":{"anyOf":[{"type":"number"},{"type":"null"}]},"I":{"type":"number"},"S":{"type":"number"},"dense_rank":{"type":"integer","minimum":1,"maximum":1000},"sparse_rank":{"type":"integer","minimum":1,"maximum":1000},"text_rank":{"type":"integer","minimum":1,"maximum":1000},"distance":{"type":"number"},"bm25":{"type":"number"},"sparse_score":{"type":"number"},"fused_rank":{"type":"integer","minimum":1,"maximum":1000},"fused_score":{"type":"number"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryView":{"type":"object","required":["id","revision","scope","kind","content","observed_at_ms","created_at_ms","updated_at_ms","importance","status","stale","segment","evidence","scores"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"revision":{"$ref":"#/components/schemas/MemoryU64"},"scope":{"$ref":"#/components/schemas/MemoryEffectiveScope"},"kind":{"$ref":"#/components/schemas/MemoryKind"},"content":{"type":"string","maxLength":4096},"observed_at_ms":{"$ref":"#/components/schemas/MemoryI64"},"created_at_ms":{"$ref":"#/components/schemas/MemoryU64"},"updated_at_ms":{"$ref":"#/components/schemas/MemoryU64"},"importance":{"type":"number"},"confidence":{"type":"number"},"expires_at_ms":{"$ref":"#/components/schemas/MemoryU64"},"status":{"enum":["accepted","disputed"],"type":"string"},"stale":{"type":"boolean"},"segment":{"anyOf":[{"$ref":"#/components/schemas/MemoryInterval"},{"type":"null"}]},"evidence":{"type":"array","items":{"$ref":"#/components/schemas/MemoryEvidenceView"},"maxItems":64},"scores":{"$ref":"#/components/schemas/MemoryScores"},"provenance":{"type":"array","items":{"$ref":"#/components/schemas/MemoryProvenance"},"maxItems":64},"references":{"type":"array","items":{"$ref":"#/components/schemas/MemoryRef"},"maxItems":256},"paths":{"type":"array","items":{"$ref":"#/components/schemas/MemoryPath"},"maxItems":100},"metadata":{"$ref":"#/components/schemas/MemoryProperties"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryReadMeta":{"type":"object","required":["scopes","as_of_ms","ttl_clock_ms","sequence","complete","truncation","limitations","mode","candidates_examined","edges_charged","vector_components","mmr_components","evidence_bytes"],"properties":{"scopes":{"type":"array","items":{"$ref":"#/components/schemas/MemoryEffectiveScope"},"minItems":1,"maxItems":16},"as_of_ms":{"$ref":"#/components/schemas/MemoryI64"},"ttl_clock_ms":{"$ref":"#/components/schemas/MemoryU64"},"sequence":{"$ref":"#/components/schemas/MemoryU64"},"complete":{"type":"boolean"},"truncation":{"type":"array","items":{"enum":["candidates","source_scan","vector_components","evidence_bytes","mmr_components","visited_nodes","edges","degree","deadline","output","deletion_barrier"],"type":"string"}},"limitations":{"type":"array","items":{"enum":["historical_candidate_discovery_not_exhaustive","retained_state_only"],"type":"string"}},"mode":{"enum":["exact_enumeration","approximate","bounded_expansion"],"type":"string"},"candidates_examined":{"$ref":"#/components/schemas/MemoryU64"},"edges_charged":{"$ref":"#/components/schemas/MemoryU64"},"vector_components":{"$ref":"#/components/schemas/MemoryU64"},"mmr_components":{"$ref":"#/components/schemas/MemoryU64"},"evidence_bytes":{"$ref":"#/components/schemas/MemoryU64"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryRecallResponse":{"type":"object","required":["memories","meta","ranking_version","selection"],"properties":{"memories":{"type":"array","items":{"$ref":"#/components/schemas/MemoryView"},"maxItems":100},"meta":{"$ref":"#/components/schemas/MemoryReadMeta"},"ranking_version":{"enum":["memory-ranking-v1"],"type":"string"},"selection":{"enum":["weighted","exact","mmr"],"type":"string"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryFactView":{"type":"object","required":["series","slot","slot_revision","destination_id","interval","status","stale","evidence"],"properties":{"series":{"$ref":"#/components/schemas/MemoryRef"},"slot":{"$ref":"#/components/schemas/MemorySlot"},"slot_revision":{"$ref":"#/components/schemas/MemoryU64"},"destination_id":{"$ref":"#/components/schemas/MemoryKey"},"interval":{"$ref":"#/components/schemas/MemoryInterval"},"status":{"enum":["accepted","disputed"],"type":"string"},"stale":{"type":"boolean"},"evidence":{"type":"array","items":{"$ref":"#/components/schemas/MemoryEvidenceView"},"maxItems":64},"provenance":{"type":"array","items":{"$ref":"#/components/schemas/MemoryProvenance"},"maxItems":64}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryFactsResponse":{"type":"object","required":["facts","meta"],"properties":{"facts":{"type":"array","items":{"$ref":"#/components/schemas/MemoryFactView"},"maxItems":100},"meta":{"$ref":"#/components/schemas/MemoryReadMeta"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryCompeting":{"type":"object","required":["id","interval","revision"],"properties":{"id":{"$ref":"#/components/schemas/MemoryKey"},"interval":{"$ref":"#/components/schemas/MemoryInterval"},"revision":{"$ref":"#/components/schemas/MemoryU64"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryErrorDetail":{"type":"object","properties":{"field":{"$ref":"#/components/schemas/MemoryKey"},"limit":{"$ref":"#/components/schemas/MemoryU64"},"requested":{"$ref":"#/components/schemas/MemoryU64"},"competing":{"type":"array","items":{"$ref":"#/components/schemas/MemoryCompeting"},"maxItems":256},"receipt_lookup_required":{"type":"boolean"},"code":{"type":"string"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryErrorContext":{"type":"object","properties":{"field":{"type":"string"},"limit":{"$ref":"#/components/schemas/MemoryU64"},"requested":{"$ref":"#/components/schemas/MemoryU64"},"feature":{"type":"string"},"category":{"type":"string"}},"additionalProperties":false},"##,
        "\n"
    ),
    concat!(
        r##"      "MemoryError":{"type":"object","required":["error"],"properties":{"error":{"type":"object","required":["code","message"],"properties":{"code":{"type":"string"},"message":{"type":"string"},"details":{"$ref":"#/components/schemas/MemoryErrorContext"}},"additionalProperties":false}},"additionalProperties":false},"##,
        "\n"
    ),
    // Ordinary collection KV: two paths (four operations) and ten wire schemas.
    concat!(
        r##"    "/v1/collections/{collection}/kv/entry":{"parameters":[{"name":"collection","in":"path","required":true,"schema":{"type":"string"}}],"get":{"operationId":"KvGet","summary":"Read a live key-value entry","description":"Uses an existing one-dimension collection; never creates it. Tenant and immutable collection bindings apply before lookup. Namespaces are application partitions, not authorization boundaries. All successes are HTTP 200. Unlisted methods return 405; unknown routes return 404. Requires collection read scope; expiry is checked at the page/read clock.","responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/KvEntry"}}}},"400":{"description":"invalid_query for unknown, duplicate or malformed query parameters; invalid_json for malformed JSON.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"401":{"$ref":"#/components/responses/Unauthorized"},"403":{"description":"read_only or forbidden_scope: operation, tenant or collection binding refused before lookup.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"404":{"description":"collection_not_found for an authorized missing collection; kv_not_found for an absent/due entry on KvGet.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"409":{"description":"not_kv_collection for dimensions other than one; kv_slots_exhausted for 16 occupied probe slots; upgrade_required for expiry before format v5.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"412":{"description":"precondition_failed: the expected live version/absence does not match; nothing was written.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"413":{"$ref":"#/components/responses/TooLarge"},"415":{"$ref":"#/components/responses/MediaType"},"422":{"description":"invalid_json for unknown/duplicate JSON fields or wrong types; invalid_input for canonical decimal/base64 or byte/limit bounds.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"429":{"description":"rate_limited or quota_exceeded; quotas share ordinary record accounting, including physically retained expired records.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}}},"503":{"$ref":"#/components/responses/Unavailable"}},"parameters":[{"name":"namespace","in":"query","required":true,"schema":{"$ref":"#/components/schemas/KvNamespace"}},{"name":"key","in":"query","required":true,"schema":{"$ref":"#/components/schemas/KvKey"}}]},"put":{"operationId":"KvPut","summary":"Put a byte value with an atomic expected-version check","description":"Uses an existing one-dimension collection; never creates it. Tenant and immutable collection bindings apply before lookup. Namespaces are application partitions, not authorization boundaries. All successes are HTTP 200. Unlisted methods return 405; unknown routes return 404. Requires collection write scope. Unknown/duplicate JSON fields are rejected. No query parameters. Ordinary KV uses configured durability: full syncs, periodic can lose a flush interval, none is memory-only. Indeterminate outcomes must not be blindly retried.","responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/KvPutResult"}}}},"400":{"description":"invalid_query for unknown, duplicate or malformed query parameters; invalid_json for malformed JSON.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"401":{"$ref":"#/components/responses/Unauthorized"},"403":{"description":"read_only or forbidden_scope: operation, tenant or collection binding refused before lookup.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"404":{"description":"collection_not_found for an authorized missing collection; kv_not_found for an absent/due entry on KvGet.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"409":{"description":"not_kv_collection for dimensions other than one; kv_slots_exhausted for 16 occupied probe slots; upgrade_required for expiry before format v5.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"412":{"description":"precondition_failed: the expected live version/absence does not match; nothing was written.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"413":{"$ref":"#/components/responses/TooLarge"},"415":{"$ref":"#/components/responses/MediaType"},"422":{"description":"invalid_json for unknown/duplicate JSON fields or wrong types; invalid_input for canonical decimal/base64 or byte/limit bounds.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"429":{"description":"rate_limited or quota_exceeded; quotas share ordinary record accounting, including physically retained expired records.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}}},"503":{"$ref":"#/components/responses/Unavailable"}},"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/KvPut"}}}}},"delete":{"operationId":"KvDelete","summary":"Delete a key-value entry with an atomic expected-version check","description":"Uses an existing one-dimension collection; never creates it. Tenant and immutable collection bindings apply before lookup. Namespaces are application partitions, not authorization boundaries. All successes are HTTP 200. Unlisted methods return 405; unknown routes return 404. Requires collection write scope. A missing/due entry returns deleted=false for any or absent; revision mismatches return 412. Follows configured durability, not an unconditional sync.","responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/KvDeleteResult"}}}},"400":{"description":"invalid_query for unknown, duplicate or malformed query parameters; invalid_json for malformed JSON.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"401":{"$ref":"#/components/responses/Unauthorized"},"403":{"description":"read_only or forbidden_scope: operation, tenant or collection binding refused before lookup.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"404":{"description":"collection_not_found for an authorized missing collection; kv_not_found for an absent/due entry on KvGet.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"409":{"description":"not_kv_collection for dimensions other than one; kv_slots_exhausted for 16 occupied probe slots; upgrade_required for expiry before format v5.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"412":{"description":"precondition_failed: the expected live version/absence does not match; nothing was written.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"413":{"$ref":"#/components/responses/TooLarge"},"415":{"$ref":"#/components/responses/MediaType"},"422":{"description":"invalid_json for unknown/duplicate JSON fields or wrong types; invalid_input for canonical decimal/base64 or byte/limit bounds.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"429":{"description":"rate_limited or quota_exceeded; quotas share ordinary record accounting, including physically retained expired records.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}}},"503":{"$ref":"#/components/responses/Unavailable"}},"parameters":[{"name":"namespace","in":"query","required":true,"schema":{"$ref":"#/components/schemas/KvNamespace"}},{"name":"key","in":"query","required":true,"schema":{"$ref":"#/components/schemas/KvKey"}},{"name":"expected","in":"query","required":false,"schema":{"type":"string","default":"any","pattern":"^(any|absent|0|[1-9][0-9]{0,19})$","description":"any, absent, or canonical uint64 decimal at most 18446744073709551615."}}]}},"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/collections/{collection}/kv/entries":{"parameters":[{"name":"collection","in":"path","required":true,"schema":{"type":"string"}}],"get":{"operationId":"KvList","summary":"List a bounded live prefix page","description":"Uses an existing one-dimension collection; never creates it. Tenant and immutable collection bindings apply before lookup. Namespaces are application partitions, not authorization boundaries. All successes are HTTP 200. Unlisted methods return 405; unknown routes return 404. Requires collection read scope. The canonical compact JSON body {\"items\":[...],\"next_after\":...} never exceeds 16777216 bytes. Returns the largest prefix fitting both limit and this boundary, ordered by unsigned UTF-8 bytes. after is exclusive. One owner guard and expiry clock per page, no snapshot across pages. next_after is the last returned key iff another live matching entry exists at that clock; an empty page never has a continuation.","responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/KvPage"}}}},"400":{"description":"invalid_query for unknown, duplicate or malformed query parameters; invalid_json for malformed JSON.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"401":{"$ref":"#/components/responses/Unauthorized"},"403":{"description":"read_only or forbidden_scope: operation, tenant or collection binding refused before lookup.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"404":{"description":"collection_not_found for an authorized missing collection; kv_not_found for an absent/due entry on KvGet.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"409":{"description":"not_kv_collection for dimensions other than one; kv_slots_exhausted for 16 occupied probe slots; upgrade_required for expiry before format v5.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"412":{"description":"precondition_failed: the expected live version/absence does not match; nothing was written.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"413":{"$ref":"#/components/responses/TooLarge"},"415":{"$ref":"#/components/responses/MediaType"},"422":{"description":"invalid_json for unknown/duplicate JSON fields or wrong types; invalid_input for canonical decimal/base64 or byte/limit bounds.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"429":{"description":"rate_limited or quota_exceeded; quotas share ordinary record accounting, including physically retained expired records.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}}},"503":{"$ref":"#/components/responses/Unavailable"}},"parameters":[{"name":"namespace","in":"query","required":true,"schema":{"$ref":"#/components/schemas/KvNamespace"}},{"name":"prefix","in":"query","required":false,"schema":{"type":"string","default":"","maxLength":1024,"x-maxUtf8Bytes":1024}},{"name":"after","in":"query","required":false,"schema":{"$ref":"#/components/schemas/KvKey"}},{"name":"limit","in":"query","required":false,"schema":{"type":"integer","minimum":1,"maximum":10000,"default":100,"description":"Canonical decimal query string."}}]}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvDecimal":{"type":"string","pattern":"^(0|[1-9][0-9]{0,19})$","x-maximum":"18446744073709551615","description":"Canonical uint64 decimal string, at most 18446744073709551615. Issued versions are nonzero; expected revision zero is valid."},"##,
        "\n"
    ),
    concat!(
        r##"      "KvNamespace":{"type":"string","minLength":1,"maxLength":255,"x-maxUtf8Bytes":255,"description":"1..255 UTF-8 bytes; no normalization. Not an authorization boundary."},"##,
        "\n"
    ),
    concat!(
        r##"      "KvKey":{"type":"string","minLength":1,"maxLength":1024,"x-maxUtf8Bytes":1024,"description":"1..1024 UTF-8 bytes; unsigned byte lexical ordering."},"##,
        "\n"
    ),
    concat!(
        r##"      "KvValue":{"type":"string","maxLength":1396388,"contentEncoding":"base64","pattern":"^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$","x-maxDecodedBytes":1047290,"description":"Canonical padded RFC 4648 standard base64, including zero unused pad bits. Empty is valid; at most 1047290 decoded bytes."},"##,
        "\n"
    ),
    concat!(
        r##"      "KvExpected":{"oneOf":[{"enum":["any","absent"]},{"type":"object","additionalProperties":false,"required":["revision"],"properties":{"revision":{"$ref":"#/components/schemas/KvDecimal"}}}],"default":"any","description":"any replaces/deletes without a precondition; absent requires no live entry; revision requires that exact live version. A mismatch returns 412."},"##,
        "\n"
    ),
    concat!(
        r##"      "KvPut":{"type":"object","additionalProperties":false,"required":["namespace","key","value_base64"],"properties":{"namespace":{"$ref":"#/components/schemas/KvNamespace"},"key":{"$ref":"#/components/schemas/KvKey"},"value_base64":{"$ref":"#/components/schemas/KvValue"},"expected":{"$ref":"#/components/schemas/KvExpected"},"expires_at_ms":{"anyOf":[{"$ref":"#/components/schemas/KvDecimal"},{"type":"null"}],"description":"Absolute Unix milliseconds. Omitted/null clears expiry; an already-due put succeeds then reads absent. Needs format v5 when set."}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvEntry":{"type":"object","additionalProperties":false,"required":["key","value_base64","version","expires_at_ms"],"properties":{"key":{"$ref":"#/components/schemas/KvKey"},"value_base64":{"$ref":"#/components/schemas/KvValue"},"version":{"$ref":"#/components/schemas/KvDecimal"},"expires_at_ms":{"anyOf":[{"$ref":"#/components/schemas/KvDecimal"},{"type":"null"}]}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvPutResult":{"type":"object","additionalProperties":false,"required":["version","expires_at_ms"],"properties":{"version":{"$ref":"#/components/schemas/KvDecimal"},"expires_at_ms":{"anyOf":[{"$ref":"#/components/schemas/KvDecimal"},{"type":"null"}]}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvDeleteResult":{"type":"object","additionalProperties":false,"required":["deleted"],"properties":{"deleted":{"type":"boolean"}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvPage":{"type":"object","additionalProperties":false,"required":["items","next_after"],"properties":{"items":{"type":"array","maxItems":10000,"items":{"$ref":"#/components/schemas/KvEntry"}},"next_after":{"anyOf":[{"$ref":"#/components/schemas/KvKey"},{"type":"null"}]}}},"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/collections/{collection}/kv/batch":{"parameters":[{"name":"collection","in":"path","required":true,"schema":{"type":"string"}}],"post":{"operationId":"KvBatch","summary":"Apply puts and deletes of one collection atomically","description":"Uses an existing one-dimension collection; never creates it. Tenant and immutable collection bindings apply before lookup. Namespaces are application partitions, not authorization boundaries. All successes are HTTP 200. Unlisted methods return 405; unknown routes return 404. Requires collection write scope. At most 1000 operations whose decoded put values total at most 4194304 bytes; no two operations name the same namespace and key; an empty batch is a successful no-op. The operations behave as if they ran one at a time, in order, on one clock, and commit as one engine frame: all or none, and a reader sees the state before the batch or the state after it. A put has no expires_at_ms and clears the deadline of the entry it replaces; every put of one batch returns the same version. The request is checked as a whole first (422); then the first operation that cannot run fails the batch and nothing is written: precondition_failed (412, naming no operation) or kv_slots_exhausted (409). The live-record quota is checked once over all slot changes, and only when the batch writes a record. Follows the configured durability: full syncs, periodic can lose one flush interval, none is memory-only. A commit_indeterminate outcome proves nothing: read the keys back with the get-many route before any retry. A server older than this route answers 404 not_found to the admin token and 403 to every other credential; look for this path in the routes of GET /v1/capabilities, not at features.kv_batch.","responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/KvBatchResponse"}}}},"400":{"description":"invalid_query for unknown, duplicate or malformed query parameters; invalid_json for malformed JSON.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"401":{"$ref":"#/components/responses/Unauthorized"},"403":{"description":"read_only or forbidden_scope: operation, tenant or collection binding refused before lookup.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"404":{"description":"collection_not_found for an authorized missing collection.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"409":{"description":"not_kv_collection for dimensions other than one; kv_slots_exhausted for 16 occupied probe slots; graph_attached_record for a delete of a record attached to a graph node; upgrade_required for a file that predates batches.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"412":{"description":"precondition_failed: an expected live version or absence does not match; nothing was written.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"413":{"$ref":"#/components/responses/TooLarge"},"415":{"$ref":"#/components/responses/MediaType"},"422":{"description":"invalid_json for unknown/duplicate JSON fields or wrong types; invalid_input for canonical decimal/base64 or byte bounds, more than 1000 operations, more than 4194304 value bytes, or a repeated namespace and key; the message names the operation.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"429":{"description":"rate_limited or quota_exceeded; quotas share ordinary record accounting, including physically retained expired records.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}}},"503":{"$ref":"#/components/responses/Unavailable"}},"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/KvBatchRequest"}}}}}},"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/collections/{collection}/kv/entries/get":{"parameters":[{"name":"collection","in":"path","required":true,"schema":{"type":"string"}}],"post":{"operationId":"KvGetMany","summary":"Read up to 1000 named entries in one call","description":"Uses an existing one-dimension collection; never creates it. Tenant and immutable collection bindings apply before lookup. Namespaces are application partitions, not authorization boundaries. All successes are HTTP 200. Unlisted methods return 405; unknown routes return 404. Requires collection read scope; a read-only credential may use this POST. 0 to 1000 keys, repeats allowed. items has one element for each of the first len(items) keys, in order: the entry, or null for a key that is absent or due. The canonical compact JSON body never exceeds 16777216 bytes: the longest prefix of the keys that fits is returned and next_index is the index of the first key not read (at least 1, equal to len(items)); it is null when every key was read. One owner guard and expiry clock serve a call; there is no snapshot across calls. A server older than this route answers 404 not_found to the admin token and 403 to every other credential; look for this path in the routes of GET /v1/capabilities.","responses":{"200":{"description":"Operation result","content":{"application/json":{"schema":{"$ref":"#/components/schemas/KvGetManyResponse"}}}},"400":{"description":"invalid_query for unknown, duplicate or malformed query parameters; invalid_json for malformed JSON.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"401":{"$ref":"#/components/responses/Unauthorized"},"403":{"description":"read_only or forbidden_scope: operation, tenant or collection binding refused before lookup.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"404":{"description":"collection_not_found for an authorized missing collection.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"409":{"description":"not_kv_collection for dimensions other than one.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"413":{"$ref":"#/components/responses/TooLarge"},"415":{"$ref":"#/components/responses/MediaType"},"422":{"description":"invalid_json for unknown/duplicate JSON fields or wrong types; invalid_input for byte bounds or more than 1000 keys; the message names the key.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"429":{"description":"rate_limited or quota_exceeded; quotas share ordinary record accounting, including physically retained expired records.","content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}},"headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}}},"503":{"$ref":"#/components/responses/Unavailable"}},"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/KvGetManyRequest"}}}}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvKeyRef":{"type":"object","additionalProperties":false,"required":["namespace","key"],"properties":{"namespace":{"$ref":"#/components/schemas/KvNamespace"},"key":{"$ref":"#/components/schemas/KvKey"}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvGetManyRequest":{"type":"object","additionalProperties":false,"required":["keys"],"properties":{"keys":{"type":"array","maxItems":1000,"items":{"$ref":"#/components/schemas/KvKeyRef"}}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvGetManyResponse":{"type":"object","additionalProperties":false,"required":["items","next_index"],"properties":{"items":{"type":"array","maxItems":1000,"items":{"anyOf":[{"$ref":"#/components/schemas/KvEntry"},{"type":"null"}]}},"next_index":{"type":["integer","null"],"minimum":1,"maximum":999,"description":"null when every key was read; otherwise the index of the first key not read, equal to the number of items."}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvBatchPut":{"type":"object","additionalProperties":false,"required":["op","namespace","key","value_base64"],"properties":{"op":{"const":"put"},"namespace":{"$ref":"#/components/schemas/KvNamespace"},"key":{"$ref":"#/components/schemas/KvKey"},"value_base64":{"$ref":"#/components/schemas/KvValue"},"expected":{"$ref":"#/components/schemas/KvExpected"}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvBatchDelete":{"type":"object","additionalProperties":false,"required":["op","namespace","key"],"properties":{"op":{"const":"delete"},"namespace":{"$ref":"#/components/schemas/KvNamespace"},"key":{"$ref":"#/components/schemas/KvKey"},"expected":{"$ref":"#/components/schemas/KvExpected"}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvBatchRequest":{"type":"object","additionalProperties":false,"required":["operations"],"properties":{"operations":{"type":"array","maxItems":1000,"items":{"oneOf":[{"$ref":"#/components/schemas/KvBatchPut"},{"$ref":"#/components/schemas/KvBatchDelete"}]}}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvBatchPutResult":{"type":"object","additionalProperties":false,"required":["version"],"properties":{"version":{"$ref":"#/components/schemas/KvDecimal"}}},"##,
        "\n"
    ),
    concat!(
        r##"      "KvBatchResponse":{"type":"object","additionalProperties":false,"required":["results"],"properties":{"results":{"type":"array","maxItems":1000,"description":"One result for each operation, in order: a put gives its version, the same for every put of the batch; a delete gives whether a live entry was deleted.","items":{"oneOf":[{"$ref":"#/components/schemas/KvBatchPutResult"},{"$ref":"#/components/schemas/KvDeleteResult"}]}}}},"##,
        "\n"
    ),
    concat!(
        r##"      "ProviderAlias": {"type":"string","minLength":1,"maxLength":256,"pattern":"^[A-Za-z0-9._-]{1,256}$","description":"Validated provider alias syntax. hashing and legacy http remain accepted; other aliases must be configured and allowlisted by an administrator."},"##,
        "\n"
    ),
    concat!(
        r##"      "EmbedResponse": {"type":"object","additionalProperties":false,"required":["vector","dimensions","provider"],"properties":{"vector":{"$ref":"#/components/schemas/Vector"},"dimensions":{"type":"integer","minimum":1,"maximum":65536,"description":"Number of vector components; equals the vector length and the collection dimension count."},"provider":{"$ref":"#/components/schemas/ProviderAlias"}}},"##,
        "\n"
    ),
    concat!(
        r##"      "ProviderNotConfiguredError": {"description":"The requested provider alias is not enabled for the authorized collection.","allOf":[{"$ref":"#/components/schemas/Error"},{"type":"object","properties":{"error":{"type":"object","properties":{"code":{"const":"provider_not_configured"}}}}}]},"##,
        "\n"
    ),
    concat!(
        r##"      "ProviderSnapshotConflictError": {"description":"The serving snapshot changed during the request. The response is sanitized and excludes candidate, tenant, and provider data; callers retry the whole request. No Retry-After is supplied.","allOf":[{"$ref":"#/components/schemas/Error"},{"type":"object","properties":{"error":{"type":"object","properties":{"code":{"const":"provider_snapshot_conflict"}}}}}]},"##,
        "\n"
    ),
    concat!(
        r##"      "ProviderInvalidInputError": {"description":"Provider input or custom candidate depth is invalid.","allOf":[{"$ref":"#/components/schemas/Error"},{"type":"object","properties":{"error":{"type":"object","properties":{"code":{"const":"invalid_provider_input"}}}}}]},"##,
        "\n"
    ),
    concat!(
        r##"      "ProviderInvalidResponseError": {"description":"Provider response shape, dimensions, cardinality, IDs, or scores are invalid.","allOf":[{"$ref":"#/components/schemas/Error"},{"type":"object","properties":{"error":{"type":"object","properties":{"code":{"const":"provider_invalid_response"}}}}}]},"##,
        "\n"
    ),
    concat!(
        r##"      "ProviderUnavailableError": {"description":"Provider connection, authentication, or availability failed.","allOf":[{"$ref":"#/components/schemas/Error"},{"type":"object","properties":{"error":{"type":"object","properties":{"code":{"const":"provider_unavailable"}}}}}]},"##,
        "\n"
    ),
    concat!(
        r##"      "ProviderTimeoutError": {"description":"The provider call exceeded its total deadline.","allOf":[{"$ref":"#/components/schemas/Error"},{"type":"object","properties":{"error":{"type":"object","properties":{"code":{"const":"provider_timeout"}}}}}]},"##,
        "\n"
    ),
    concat!(
        r##"      "ProviderBusyError": {"description":"Provider or tenant admission is full; Retry-After is supplied.","allOf":[{"$ref":"#/components/schemas/Error"},{"type":"object","properties":{"error":{"type":"object","properties":{"code":{"const":"provider_busy"}}}}}]}"##,
        "\n"
    ),
    concat!(
        r##"    "/v1/admin/tenants/{id}/usage":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"get":{"summary":"Admin only: live usage of one tenant","description":"Point-in-time snapshot: tenant collections (unqualified names) with live vector counts, the aggregate against max_vectors with remaining headroom, and bound credential counts by state.","responses":{"200":{"description":"Tenant usage","content":{"application/json":{"schema":{"$ref":"#/components/schemas/TenantUsage"}}}},"404":{"description":"tenant_not_found: unknown or deleted tenant"},"409":{"description":"tenants_disabled"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/admin/tenants/{id}/settings":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"put":{"summary":"Admin only: rename a tenant or change its aggregate vector limit","description":"Partial update: send name, max_vectors or both; max_vectors null removes the limit. A limit below current usage is accepted and blocks further growth until usage drops. Durable and audited (tenant_updated).","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"minProperties":1,"properties":{"name":{"type":"string","minLength":1,"maxLength":64,"description":"1..64 UTF-8 bytes without control characters; not unique"},"max_vectors":{"type":["integer","null"],"minimum":0}}}}}},"responses":{"200":{"description":"Updated tenant","content":{"application/json":{"schema":{"type":"object","required":["tenant"],"properties":{"tenant":{"$ref":"#/components/schemas/Tenant"}}}}}},"404":{"description":"tenant_not_found: unknown or deleted tenant"},"409":{"description":"tenants_disabled"},"422":{"description":"invalid_input: empty update, invalid name or invalid limit"},"503":{"description":"tenants_commit_indeterminate: the change is visible but its durability is unconfirmed; list or read usage before retrying"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/admin/tenants/{id}/suspend":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"post":{"summary":"Admin only: suspend a tenant","description":"Tenant-bound credentials are refused with 403 tenant_suspended on REST and gRPC (PERMISSION_DENIED) until the tenant is resumed, and new tenant credentials are refused with 409 tenant_suspended. Collections, vectors and credentials are kept; administrators can still read and clean up tenant collections. Idempotent; durable and audited (tenant_suspended). Servers that predate suspension refuse to start while any tenant is suspended.","responses":{"200":{"description":"Suspended tenant","content":{"application/json":{"schema":{"type":"object","required":["tenant"],"properties":{"tenant":{"$ref":"#/components/schemas/Tenant"}}}}}},"404":{"description":"tenant_not_found: unknown or deleted tenant"},"409":{"description":"tenants_disabled"},"503":{"description":"tenants_commit_indeterminate: the change is visible but its durability is unconfirmed; list or read usage before retrying"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/admin/tenants/{id}/resume":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"post":{"summary":"Admin only: resume a suspended tenant","description":"Restores access for unexpired, unrevoked tenant credentials. Idempotent; durable and audited (tenant_resumed).","responses":{"200":{"description":"Active tenant","content":{"application/json":{"schema":{"type":"object","required":["tenant"],"properties":{"tenant":{"$ref":"#/components/schemas/Tenant"}}}}}},"404":{"description":"tenant_not_found: unknown or deleted tenant"},"409":{"description":"tenants_disabled"},"503":{"description":"tenants_commit_indeterminate: the change is visible but its durability is unconfirmed; list or read usage before retrying"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/auth/config":{"get":{"security":[],"summary":"Public: which dashboard sign-in methods are available","responses":{"200":{"description":"Sign-in configuration","content":{"application/json":{"schema":{"type":"object","required":["mode","password_login","reason","setup_required"],"properties":{"mode":{"enum":["open","protected"],"description":"open means no admin token is configured"},"password_login":{"type":"boolean"},"reason":{"enum":["accounts_disabled","insecure_transport",null],"description":"Why password sign-in is unavailable"},"setup_required":{"type":"boolean","description":"No accounts exist yet: sign in with the admin token and create the first super admin"}}}}}}}}},"##,
        "\n",
        r##"    "/v1/auth/login":{"post":{"security":[],"summary":"Public: sign in with a username and password","description":"Starts a server session carried in an HttpOnly, SameSite=Strict cookie (__Host-vlite_session over HTTPS, otherwise vlite_session). Sessions live in memory, so a restart signs everyone out. Needs HTTPS, a loopback-only listener or VLITE_TRUSTED_PROXY=1, and the X-VLite-Request header; cross-site requests are refused. Unknown usernames and wrong passwords fail the same way; repeated failures lock the username (and, behind a trusted proxy, the client IP) with exponential backoff up to 15 minutes.","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"required":["username","password"],"properties":{"username":{"type":"string","maxLength":64,"description":"Case-insensitive"},"password":{"type":"string","maxLength":128}}}}}},"responses":{"200":{"description":"Signed in","headers":{"Set-Cookie":{"description":"Session cookie","schema":{"type":"string"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/AuthSignedIn"}}}},"401":{"description":"invalid_credentials"},"403":{"description":"account_disabled, insecure_transport, csrf_rejected or tenant_suspended"},"409":{"description":"accounts_disabled (needs an admin token and VLITE_ADMIN_DIR) or accounts_require_upgrade"},"429":{"description":"too_many_attempts","headers":{"Retry-After":{"description":"Seconds until the next attempt is accepted","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"503":{"description":"busy (too many concurrent password checks) or audit_unavailable"}}}},"##,
        "\n",
        r##"    "/v1/auth/session":{"get":{"summary":"Who is signed in, and how","description":"Works for session cookies, bearer tokens and open mode, including while a password change is pending.","responses":{"200":{"description":"Current session","content":{"application/json":{"schema":{"$ref":"#/components/schemas/AuthSession"}}}},"401":{"description":"Missing or invalid token, or session_expired (the response also clears the session cookie)"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/auth/logout":{"post":{"summary":"Sign out: end this session and clear the cookie","description":"Token callers get signed_out false. Cookie requests must send X-VLite-Request.","responses":{"200":{"description":"Signed out","headers":{"Set-Cookie":{"description":"Expires both session cookie names","schema":{"type":"string"}}},"content":{"application/json":{"schema":{"type":"object","required":["signed_out"],"properties":{"signed_out":{"type":"boolean"}}}}}},"401":{"description":"Missing or invalid token, or session_expired"},"403":{"description":"csrf_rejected"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/auth/password":{"post":{"summary":"Change your own password","description":"Session cookie only. Ends every session of the account and starts a new one for this browser. Passwords are 15 to 128 characters (passphrases welcome); the username, common words and simple sequences do not count toward the length. Durable and audited (user_password_changed).","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"required":["current_password","new_password"],"properties":{"current_password":{"type":"string"},"new_password":{"type":"string","minLength":15,"maxLength":128}}}}}},"responses":{"200":{"description":"Password changed; new session cookie","headers":{"Set-Cookie":{"description":"Session cookie","schema":{"type":"string"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/AuthSignedIn"}}}},"401":{"description":"session_expired"},"403":{"description":"wrong_password or csrf_rejected"},"409":{"description":"not_signed_in (token or open mode) or account_changed"},"422":{"description":"weak_password or password_unchanged"},"429":{"description":"too_many_attempts","headers":{"Retry-After":{"description":"Seconds until the next attempt is accepted","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"503":{"description":"accounts_unavailable (nothing changed), accounts_commit_indeterminate (in effect but durability unconfirmed; list accounts before retrying), audit_unavailable or busy"}}}},"##,
        "\n",
        r##"    "/v1/admin/users":{"get":{"summary":"Admin only: list dashboard accounts","responses":{"200":{"description":"Accounts sorted by username","content":{"application/json":{"schema":{"type":"object","required":["items","capacity"],"properties":{"items":{"type":"array","items":{"$ref":"#/components/schemas/UserAccount"}},"capacity":{"const":1024}}}}}},"409":{"description":"accounts_disabled"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}},"post":{"summary":"Admin only: create a dashboard account","description":"Super admins have full admin rights and no tenant. Editors (read, search, write, import, index and collection management) and viewers (read and search) may be confined to one tenant and need a format v4+ database. must_change_password defaults to true. Durable and audited (user_created); passwords are never logged or returned.","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"required":["username","password","role"],"properties":{"username":{"type":"string","minLength":3,"maxLength":64,"description":"Letters, digits, '.', '_', '-' and '@', starting with a letter or digit; stored lowercased"},"password":{"type":"string","minLength":15,"maxLength":128},"role":{"enum":["super_admin","editor","viewer"]},"tenant_id":{"type":["string","null"],"pattern":"^[a-f0-9]{32}$"},"must_change_password":{"type":"boolean","default":true},"disabled":{"type":"boolean","default":false},"graphs":{"type":["array","null"],"maxItems":64,"items":{"type":"string","pattern":"^(\\*|[a-z][a-z0-9_-]{0,62})$"},"description":"Graphs the account may use: null = role default (every graph in its tenant scope), [] = none, or 1-64 names (or [\"*\"]). Tenanted accounts use tenant-local names of at most 28 bytes. Not allowed for super_admin."},"graph_access":{"enum":["none","read","write",null],"description":"Caps graph operations below the role default (editor write, viewer read); null = role default. Viewers cannot have write; not allowed for super_admin."}}}}}},"responses":{"201":{"description":"Created account","content":{"application/json":{"schema":{"type":"object","required":["user"],"properties":{"user":{"$ref":"#/components/schemas/UserAccount"}}}}}},"409":{"description":"username_taken, accounts_full (1024 accounts), accounts_disabled or accounts_require_upgrade"},"422":{"description":"invalid_username, weak_password, tenant_not_found or invalid_input"},"503":{"description":"accounts_unavailable (nothing changed), accounts_commit_indeterminate (in effect but durability unconfirmed; list accounts before retrying), audit_unavailable or busy"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/admin/users/{id}":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"put":{"summary":"Admin only: change an account's username, role, tenant or status","description":"Partial update; tenant_id null removes the tenant. Any effective change ends the account's sessions. Your own account cannot be changed here (cannot_modify_self). Durable and audited (user_updated, with the changed fields).","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"minProperties":1,"properties":{"username":{"type":"string","minLength":3,"maxLength":64},"role":{"enum":["super_admin","editor","viewer"]},"tenant_id":{"type":["string","null"],"pattern":"^[a-f0-9]{32}$"},"disabled":{"type":"boolean"},"must_change_password":{"type":"boolean"},"graphs":{"type":["array","null"],"maxItems":64,"items":{"type":"string","pattern":"^(\\*|[a-z][a-z0-9_-]{0,62})$"},"description":"Graphs the account may use: null resets to the role default (every graph in its tenant scope), [] = none, or 1-64 names (or [\"*\"]). Tenanted accounts use tenant-local names of at most 28 bytes. Not allowed for super_admin."},"graph_access":{"enum":["none","read","write",null],"description":"Caps graph operations below the role default (editor write, viewer read); null resets to the role default. Viewers cannot have write; not allowed for super_admin."}}}}}},"responses":{"200":{"description":"Updated account and the fields that changed (empty when nothing changed)","content":{"application/json":{"schema":{"type":"object","required":["user","changed"],"properties":{"user":{"$ref":"#/components/schemas/UserAccount"},"changed":{"type":"array","items":{"enum":["username","role","tenant_id","disabled","must_change_password","graphs","graph_access"]}}}}}}},"404":{"description":"user_not_found"},"409":{"description":"cannot_modify_self, username_taken, accounts_disabled or accounts_require_upgrade"},"422":{"description":"invalid_username, tenant_not_found or invalid_input"},"503":{"description":"accounts_unavailable (nothing changed), accounts_commit_indeterminate (in effect but durability unconfirmed; list accounts before retrying), audit_unavailable or busy"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}},"delete":{"summary":"Admin only: delete an account and end its sessions","description":"Your own account cannot be deleted (cannot_modify_self). Durable and audited (user_deleted).","responses":{"200":{"description":"deleted true","content":{"application/json":{"schema":{"type":"object","required":["deleted"],"properties":{"deleted":{"const":true}}}}}},"404":{"description":"user_not_found"},"409":{"description":"cannot_modify_self or accounts_disabled"},"503":{"description":"accounts_unavailable (nothing changed), accounts_commit_indeterminate (in effect but durability unconfirmed; list accounts before retrying), audit_unavailable or busy"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
        r##"    "/v1/admin/users/{id}/password":{"parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string","pattern":"^[a-f0-9]{32}$"}}],"post":{"summary":"Admin only: set a new password for an account","description":"Ends the account's sessions and clears its sign-in lock. must_change_password defaults to true, so the user chooses their own password at next sign-in; share the temporary one out of band. Durable and audited (user_password_reset); passwords are never logged.","requestBody":{"required":true,"content":{"application/json":{"schema":{"type":"object","additionalProperties":false,"required":["password"],"properties":{"password":{"type":"string","minLength":15,"maxLength":128},"must_change_password":{"type":"boolean","default":true}}}}}},"responses":{"200":{"description":"Updated account","content":{"application/json":{"schema":{"type":"object","required":["user"],"properties":{"user":{"$ref":"#/components/schemas/UserAccount"}}}}}},"404":{"description":"user_not_found"},"409":{"description":"cannot_modify_self or accounts_disabled"},"422":{"description":"weak_password or invalid_input"},"503":{"description":"accounts_unavailable (nothing changed), accounts_commit_indeterminate (in effect but durability unconfirmed; list accounts before retrying), audit_unavailable or busy"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
    ),
    concat!(
        r##"      "Tenant":{"type":"object","required":["id","name","max_vectors","state"],"properties":{"id":{"type":"string","pattern":"^[a-f0-9]{32}$"},"name":{"type":"string"},"max_vectors":{"type":["integer","null"],"description":"Aggregate live-vector limit across tenant collections; null is unlimited"},"deleted":{"type":"boolean"},"state":{"enum":["active","suspended"]},"suspended_unix_ms":{"type":["integer","null"]},"collections":{"type":"integer","description":"List responses only: collections owned by the tenant"},"vectors":{"type":"integer","description":"List responses only: live vectors across tenant collections"},"users":{"type":"integer","description":"List responses only: dashboard accounts bound to the tenant"}}},"##,
        "\n",
        r##"      "TenantUsage":{"type":"object","required":["tenant","collections","vectors","max_vectors","remaining_vectors","over_quota","credentials"],"properties":{"tenant":{"$ref":"#/components/schemas/Tenant"},"collections":{"type":"array","items":{"type":"object","required":["name","physical_name","vectors"],"properties":{"name":{"type":"string"},"physical_name":{"type":"string","description":"t<tenant-id>__<name>"},"vectors":{"type":"integer"}}}},"vectors":{"type":"integer"},"max_vectors":{"type":["integer","null"]},"remaining_vectors":{"type":["integer","null"],"description":"Saturates at 0; null when unlimited"},"over_quota":{"type":"boolean","description":"True when usage exceeds a lowered limit"},"credentials":{"type":"object","required":["active","expired","revoked"],"properties":{"active":{"type":"integer"},"expired":{"type":"integer"},"revoked":{"type":"integer"}}}}},"##,
        "\n",
        r##"      "CapabilityIdentity":{"type":"object","description":"Returned as identity by GET /v1/capabilities for display only; authorization uses role and scope. Labels never contain secrets.","required":["kind","label"],"properties":{"kind":{"enum":["super_admin","open","read_only","scoped","user"],"description":"open means no admin token is configured and every caller is admin; user means a dashboard password session"},"label":{"type":"string"},"credential_id":{"type":"string"},"credential_name":{"type":"string"},"tenant_id":{"type":["string","null"]},"tenant_name":{"type":["string","null"]},"user_id":{"type":"string"},"username":{"type":"string"},"account_role":{"enum":["super_admin","editor","viewer"]},"must_change_password":{"type":"boolean"}}},"##,
        "\n",
        r##"      "UserAccount":{"type":"object","description":"A dashboard account; password hashes are never returned.","required":["id","username","role","role_label","tenant_id","tenant_name","disabled","must_change_password","created_unix_ms","updated_unix_ms"],"properties":{"id":{"type":"string","pattern":"^[a-f0-9]{32}$"},"username":{"type":"string"},"role":{"enum":["super_admin","editor","viewer"]},"role_label":{"type":"string"},"tenant_id":{"type":["string","null"]},"tenant_name":{"type":["string","null"]},"disabled":{"type":"boolean"},"must_change_password":{"type":"boolean"},"created_unix_ms":{"type":"integer"},"updated_unix_ms":{"type":"integer"},"active_sessions":{"type":"integer","description":"List responses only"},"last_login_unix_ms":{"type":["integer","null"],"description":"List responses only; since the server started"},"graphs":{"type":["array","null"],"maxItems":64,"items":{"type":"string","pattern":"^(\\*|[a-z][a-z0-9_-]{0,62})$"},"description":"Stored knob: graphs the account may use: null = role default (every graph in its tenant scope), [] = none, or 1-64 names (or [\"*\"]). Tenanted accounts use tenant-local names of at most 28 bytes."},"graph_access":{"enum":["none","read","write",null],"description":"Stored knob that caps graph operations below the role default (editor write, viewer read); null = role default."}}},"##,
        "\n",
        r##"      "AuthSession":{"type":"object","required":["method","role","user","expires_unix_ms","idle_timeout_seconds","must_change_password","accounts","setup_required"],"properties":{"method":{"enum":["password","token","open"]},"role":{"enum":["admin","read_only","scoped"]},"user":{"oneOf":[{"$ref":"#/components/schemas/UserAccount"},{"type":"null"}]},"expires_unix_ms":{"type":["integer","null"],"description":"Absolute session end; null for tokens and open mode"},"idle_timeout_seconds":{"type":["integer","null"]},"must_change_password":{"type":"boolean"},"accounts":{"type":"boolean","description":"Password sign-in is configured"},"setup_required":{"type":"boolean","description":"No accounts exist yet"}}},"##,
        "\n",
        r##"      "AuthSignedIn":{"type":"object","required":["user","expires_unix_ms","idle_timeout_seconds","must_change_password"],"properties":{"user":{"$ref":"#/components/schemas/UserAccount"},"expires_unix_ms":{"type":"integer"},"idle_timeout_seconds":{"type":"integer"},"must_change_password":{"type":"boolean","description":"While true, everything except /v1/auth/session, /v1/auth/logout, /v1/auth/password and GET /v1/capabilities returns 403 password_change_required"}}},"##,
        "\n",
    ),
    // Restart-free memory grants (feature-memory-live-grants ITEM-008): one
    // admin-only path that reloads the grants of the configured bindings file.
    concat!(
        r##"    "/v1/admin/memory/bindings/reload": {"post":{"summary":"Admin only: reload memory grants from the configured bindings file","description":"Rereads the configured memory bindings file and applies a grants-only change (scopes, read scopes, principals and raw_access, with a higher generation) without a restart. It runs the checks of a restart, writes the binding records and provisions the granted scopes before serving the new grants. The request body is ignored and the file is never written. An unchanged file is rechecked and installs nothing. Any other change (a binding added, removed or renamed; its tenant, graph, evidence collection, receipt window or state) needs a restart. Requests that consulted older grants may finish under them: drained is false when some were still running after 10 seconds, and the change is in effect; send the request again to wait again. A read-only database or replica installs the grants and writes nothing. A failed reload leaves the grants unchanged and is not a revocation. Answers, log lines and the audit event (memory_bindings_reloaded) name no binding, tenant, principal, scope or path.","security":[{"BearerAuth":[]}],"responses":{"200":{"description":"Reloaded or rechecked","content":{"application/json":{"schema":{"type":"object","required":["bindings","changed","drained"],"properties":{"bindings":{"type":"integer","minimum":0,"description":"Bindings in the file"},"changed":{"type":"integer","minimum":0,"description":"Bindings whose entry differs from the grants served before; 0 for an unchanged file"},"drained":{"type":"boolean","description":"Whether every request that consulted older grants had finished"}}}}}},"403":{"description":"read_only or forbidden_scope: administrators only"},"409":{"description":"memory_reload_unavailable (no bindings file or binding configured, or a gRPC router was built: restart instead), memory_reload_in_progress, memory_restart_required (more than grants and generations changed) or memory_bindings_conflict (a restart would refuse the file, for example changed grants without a higher generation); grants unchanged"},"422":{"description":"invalid_input: the bindings file is missing, not a regular file, too large or invalid; grants unchanged"},"503":{"description":"busy, unavailable, poisoned or commit_indeterminate; grants unchanged, but records or scopes of this reload may be durable: reopen or restart the database, then retry"},"429":{"description":"Credential token bucket or live-vector quota exceeded; code rate_limited or quota_exceeded. Quotas require deleting records or reissuing configuration, not blind retries.","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}}}}},"##,
        "\n",
    ),
    // Memory preflight EPIC-002: advisory, read-only admin routes.
    concat!(
        r##"    "/v1/admin/memory/bindings/preflight": {"post":{"summary":"Admin only: preflight memory grants reload","description":"Advisory, read-only evaluation of the configured file using reload's checks under a shared database lock. The request body is ignored. Never takes the reload lock, provisions scopes, writes records, installs grants or waits for older requests. Reload rereads and rechecks; preflight cannot predict memory_reload_in_progress, provisioning or record-write failures, or intervening file or database changes. New bindings and non-grants changes still need a restart; reload is unavailable after a gRPC router was built. Responses name no binding, tenant, principal, scope, graph, collection or path. The handler adds no audit event or log line of its own.","security":[{"BearerAuth":[]}],"responses":{"200":{"description":"Evaluation completed, including refusals","content":{"application/json":{"schema":{"type":"object","required":["outcome","bindings","changed","records","error"],"properties":{"outcome":{"type":"string","enum":["reload","unchanged","restart_required","refused","unavailable","invalid"]},"bindings":{"type":["integer","null"],"minimum":0,"description":"Bindings in the file; null for unavailable or invalid"},"changed":{"type":["integer","null"],"minimum":0,"description":"Changed bindings for reload or unchanged; null for all refusal outcomes"},"records":{"type":["integer","null"],"minimum":0,"description":"Binding records reload would write; 0 on a read-only database or replica; null for all refusal outcomes"},"error":{"type":["object","null"],"required":["code","message"],"properties":{"code":{"type":"string"},"message":{"type":"string"}},"description":"Reload's fixed code and message for a refusal; null for reload or unchanged"}}}}}},"401":{"description":"unauthorized: missing or invalid authentication"},"403":{"description":"read_only or forbidden_scope: administrators only"},"429":{"description":"rate_limited: credential token bucket exceeded","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"503":{"description":"busy or unavailable: worker admission failed or database closed; storage errors use the ordinary read error mapping"},"500":{"description":"internal_error: poisoned lock or blocking task join failure; storage errors use the ordinary read error mapping"}}}},"##,
        "\n",
        r##"    "/v1/admin/memory/bindings/status": {"get":{"summary":"Admin only: memory bindings status","description":"Read-only, advisory snapshot of the served bindings and configured file. Never takes the database lock or reload lock and changes nothing. May lag a concurrent reload by one call; poll after reload until file is agrees and drained is true. Reports no in-progress state. Responses name no binding, tenant, principal, scope, graph, collection or path. The handler adds no audit event or log line of its own.","security":[{"BearerAuth":[]}],"responses":{"200":{"description":"Served bindings and file snapshot","content":{"application/json":{"schema":{"type":"object","required":["configured","reloadable","bindings","file","drained"],"properties":{"configured":{"type":"boolean","description":"The served view has a configured binding"},"reloadable":{"type":"boolean","description":"Same availability as features.memory_bindings_reload"},"bindings":{"type":"integer","minimum":0,"description":"Number of served bindings"},"file":{"type":"string","enum":["agrees","differs","unreadable","none"],"description":"Comparison with the served view; none means no configured source"},"drained":{"type":"boolean","description":"Every request that consulted older grants has finished"}}}}}},"401":{"description":"unauthorized: missing or invalid authentication"},"403":{"description":"read_only or forbidden_scope: administrators only"},"429":{"description":"rate_limited: credential token bucket exceeded","headers":{"Retry-After":{"description":"Conservative retry delay in seconds (1).","schema":{"type":"integer"}}},"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Error"}}}},"500":{"description":"internal_error: blocking task join failure"}}}},"##,
        "\n",
    ),
];

/// `/openapi.json` (`info.version`) and the MCP `initialize` result
/// (`serverInfo.version`) report the package version, which release
/// preparation bumps. That one value is masked on both sides and the served
/// one must equal `CARGO_PKG_VERSION`; every other byte must still match the
/// capture, so a version bump never needs a re-pin.
const MASKED_VERSION: &str = "<package-version>";
const OPENAPI_VERSION_ANCHOR: &str = "\"title\": \"VLite REST API\",\n    \"version\": \"";

fn is_release_version(text: &str) -> bool {
    let core = text.split_once('-').map_or(text, |(core, _)| core);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Masks `info.version`; returns the masked text and the version it held.
fn mask_openapi_version(text: &str) -> Result<(String, String), String> {
    if text.matches(OPENAPI_VERSION_ANCHOR).count() != 1 {
        return Err("OpenAPI info.version must appear exactly once after the title".to_string());
    }
    let start = text.find(OPENAPI_VERSION_ANCHOR).unwrap() + OPENAPI_VERSION_ANCHOR.len();
    let length = text[start..]
        .find('"')
        .ok_or_else(|| "unterminated OpenAPI info.version".to_string())?;
    let version = &text[start..start + length];
    if !is_release_version(version) {
        return Err(format!(
            "OpenAPI info.version {version:?} is not a release version"
        ));
    }
    Ok((
        format!(
            "{}{MASKED_VERSION}{}",
            &text[..start],
            &text[start + length..]
        ),
        version.to_string(),
    ))
}

/// Masks `serverInfo.version` of an `initialize` result and returns it; other
/// responses and non-release values are left untouched (`None`).
fn mask_mcp_version(response: &mut Value) -> Option<String> {
    let slot = response.pointer_mut("/result/serverInfo/version")?;
    let version = slot.as_str()?.to_string();
    if !is_release_version(&version) {
        return None;
    }
    *slot = Value::String(MASKED_VERSION.to_string());
    Some(version)
}

/// Served OpenAPI must equal the baseline once each approved addition (if
/// present, at most once, on whole lines) is removed and the package version
/// is masked. Returns the served package version, or the first differing line
/// on failure.
fn openapi_matches_baseline(
    served: &str,
    golden: &str,
    additions: &[&str],
) -> Result<String, String> {
    let mut remainder = served.to_string();
    for addition in additions {
        let mut addition = (*addition).to_owned();
        for (baseline, reviewed) in APPROVED_OPENAPI_ADDITION_EDITS {
            match addition.matches(baseline).count() {
                0 => {}
                1 => addition = addition.replacen(baseline, reviewed, 1),
                count => {
                    return Err(format!(
                        "approved OpenAPI addition edit matched {count} times"
                    ));
                }
            }
        }
        match remainder.matches(&addition).count() {
            0 => {}
            1 => {
                let start = remainder.find(&addition).unwrap();
                if start == 0 || !remainder[..start].ends_with('\n') {
                    return Err("approved OpenAPI addition is not on whole lines".to_string());
                }
                remainder.replace_range(start..start + addition.len(), "");
            }
            count => return Err(format!("approved OpenAPI addition appears {count} times")),
        }
    }
    let (remainder, version) = mask_openapi_version(&remainder)?;
    let (golden, _) = mask_openapi_version(golden)?;
    if remainder == golden {
        return Ok(version);
    }
    let line = remainder
        .lines()
        .zip(golden.lines())
        .position(|(a, b)| a != b)
        .map_or_else(|| "length".to_string(), |index| (index + 1).to_string());
    Err(format!(
        "served /openapi.json differs from the baseline capture beyond the approved additions \
         (first difference at line {line} after removing them)"
    ))
}

/// A test-local stand-in for a reviewed addition; it exercises the allowlist
/// mechanism and is never part of `APPROVED_OPENAPI_ADDITIONS`.
const SYNTHETIC_ADDITION: &str = concat!(
    "  \"x-synthetic-addition\": {\n",
    "    \"enabled_by_default\": false\n",
    "  },\n",
);

fn golden_with_synthetic_addition() -> String {
    let golden = fixture("openapi.json");
    let anchor = "  \"servers\": [{\"url\": \"http://127.0.0.1:8080\"}],\n";
    assert_eq!(golden.matches(anchor).count(), 1);
    golden.replacen(anchor, &format!("{anchor}{SYNTHETIC_ADDITION}"), 1)
}

#[test]
fn reg008_openapi_allowlist_accepts_baseline_and_approved_additions() {
    let golden = fixture("openapi.json");
    openapi_matches_baseline(&golden, &golden, &[]).unwrap();
    openapi_matches_baseline(&golden, &golden, &[SYNTHETIC_ADDITION]).unwrap();
    let extended = golden_with_synthetic_addition();
    serde_json::from_str::<Value>(&extended).expect("synthetic addition keeps valid JSON");
    openapi_matches_baseline(&extended, &golden, &[SYNTHETIC_ADDITION]).unwrap();
    assert!(
        openapi_matches_baseline(&extended, &golden, APPROVED_OPENAPI_ADDITIONS).is_err(),
        "an addition that is not approved must fail REG-008"
    );
}

#[test]
fn reg008_openapi_allowlist_rejects_unapproved_drift() {
    let golden = fixture("openapi.json");
    let extended = golden_with_synthetic_addition();
    let addition = SYNTHETIC_ADDITION;
    // A schema change elsewhere, alongside the approved addition.
    let first_integer = extended.find("\"type\":\"integer\"").unwrap();
    let mut schema_drift = extended.clone();
    schema_drift.replace_range(
        first_integer..first_integer + "\"type\":\"integer\"".len(),
        "\"type\":\"string\"",
    );
    // An edit inside the approved block itself.
    let edited_block = extended.replacen(
        "\"enabled_by_default\": false",
        "\"enabled_by_default\": true",
        1,
    );
    // A different, unapproved additive top-level extension.
    let other_addition = extended.replacen(
        addition,
        &format!("{addition}  \"x-unapproved\": {{}},\n"),
        1,
    );
    // The approved block duplicated.
    let duplicated = extended.replacen(addition, &format!("{addition}{addition}"), 1);
    // A schema change without the approved addition.
    let bare_drift = golden.replacen("\"type\":\"integer\"", "\"type\":\"number\"", 1);
    for (name, served) in [
        ("schema drift", schema_drift),
        ("edited approved block", edited_block),
        ("unapproved addition", other_addition),
        ("duplicated approved block", duplicated),
        ("drift without addition", bare_drift),
    ] {
        assert_ne!(served, extended, "{name}: mutation must apply");
        assert!(
            openapi_matches_baseline(&served, &golden, &[addition]).is_err(),
            "{name}: must fail REG-008"
        );
    }
}

#[test]
fn reg008_package_version_is_masked_but_nothing_else() {
    let golden = fixture("openapi.json");
    let (_, baseline_version) = mask_openapi_version(&golden).unwrap();
    let anchor = format!("{OPENAPI_VERSION_ANCHOR}{baseline_version}\"");
    let with_version = |version: &str| {
        golden.replacen(&anchor, &format!("{OPENAPI_VERSION_ANCHOR}{version}\""), 1)
    };
    let bumped = with_version("9.8.7");
    assert_ne!(bumped, golden, "version bump must apply");
    assert_eq!(
        openapi_matches_baseline(&bumped, &golden, &[]),
        Ok("9.8.7".to_string())
    );
    assert_eq!(
        openapi_matches_baseline(&with_version("0.5.0-rc.1"), &golden, &[]),
        Ok("0.5.0-rc.1".to_string())
    );
    for (name, served) in [
        (
            "description drift",
            bumped.replacen(
                "\"description\": \"Experimental API",
                "\"description\": \"Stable API",
                1,
            ),
        ),
        (
            "schema drift",
            bumped.replacen("\"type\":\"integer\"", "\"type\":\"number\"", 1),
        ),
        ("version anchor duplicated", format!("{bumped}{bumped}")),
        (
            "version anchor missing",
            bumped.replacen("\"title\": \"VLite REST API\",\n", "", 1),
        ),
        ("non-release version", with_version("latest")),
        ("literal mask text", with_version(MASKED_VERSION)),
    ] {
        assert_ne!(served, bumped, "{name}: mutation must apply");
        assert!(
            openapi_matches_baseline(&served, &golden, &[]).is_err(),
            "{name}: must fail REG-008"
        );
    }
}

#[test]
fn reg008_mcp_version_is_masked_but_nothing_else() {
    let transcript: Vec<Value> = serde_json::from_str(&fixture("mcp_read_only.json")).unwrap();
    assert_eq!(
        transcript[0]["id"], 1,
        "response 1 is the initialize result"
    );
    let mut golden = transcript[0].clone();
    let mut bumped = golden.clone();
    bumped["result"]["serverInfo"]["version"] = Value::String("9.8.7".to_string());
    assert_eq!(mask_mcp_version(&mut bumped).as_deref(), Some("9.8.7"));
    assert!(mask_mcp_version(&mut golden).is_some());
    assert_eq!(bumped, golden, "only the version differed");
    let mut not_a_version = transcript[0].clone();
    not_a_version["result"]["serverInfo"]["version"] = Value::String("latest".to_string());
    assert_eq!(mask_mcp_version(&mut not_a_version), None);
    assert_eq!(not_a_version["result"]["serverInfo"]["version"], "latest");
    let mut tools_list = transcript[1].clone();
    assert_eq!(mask_mcp_version(&mut tools_list), None);
    assert_eq!(tools_list, transcript[1]);
}

#[tokio::test]
async fn reg008_served_openapi_matches_baseline_except_approved_additions() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create(directory.path().join("openapi.vlite"), 2).unwrap();
    let response = router(Service::new(db))
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let served = String::from_utf8(bytes.to_vec())
        .unwrap()
        .replace("\r\n", "\n");
    let document: Value = serde_json::from_str(&served).unwrap();
    for (path, method) in [
        ("/v1/admin/memory/bindings/preflight", "post"),
        ("/v1/admin/memory/bindings/status", "get"),
    ] {
        assert!(
            document["paths"][path][method].is_object(),
            "missing memory bindings operation: {method} {path}"
        );
    }
    // EPIC-011: graph builds merge whole graph path lines (RD-013); every
    // other served line must still match the baseline.
    let served: String = served
        .split_inclusive('\n')
        .filter(|line| {
            !line.starts_with("    \"/v1/graphs") && !line.starts_with("    \"/v1/admin/graph/")
        })
        .collect();
    let version = openapi_matches_baseline(&served, &edited_golden(), APPROVED_OPENAPI_ADDITIONS)
        .unwrap_or_else(|message| panic!("{message}"));
    assert_eq!(version, env!("CARGO_PKG_VERSION"), "served OpenAPI version");
}

/// Private Gap 2 K3a (owner-approved 2026-11-04): `tools/list` gains the
/// ordinary key-value tools, `vlite_kv_get` and `vlite_kv_list` in read-only
/// mode and all four with writes enabled. F2 (owner-approved 2026-11-05) adds
/// `vlite_kv_get_many` in both modes and `vlite_kv_batch` with writes enabled,
/// after the original four. Their exact definitions are pinned
/// in `tests/fixtures/kv/mcp-tool-additions.json`; every baseline tool and every other
/// response must still match the capture.
fn approved_mcp_additions(mode: &str) -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("kv")
        .join("mcp-tool-additions.json");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let approved: Value = serde_json::from_str(&text).unwrap();
    approved[mode].as_array().unwrap().clone()
}

/// Removes the approved additions from a served `tools/list` result and
/// returns them in served order; `None` for every other response.
fn take_mcp_additions(response: &mut Value, approved: &[Value]) -> Option<Vec<Value>> {
    let tools = response.pointer_mut("/result/tools")?.as_array_mut()?;
    let mut taken = Vec::new();
    tools.retain(|tool| {
        let listed = approved
            .iter()
            .any(|candidate| candidate["name"] == tool["name"]);
        if listed {
            taken.push(tool.clone());
        }
        !listed
    });
    Some(taken)
}

#[test]
fn reg008_mcp_allowlist_accepts_only_the_exact_approved_tools() {
    let golden: Vec<Value> = serde_json::from_str(&fixture("mcp_read_only.json")).unwrap();
    let approved = approved_mcp_additions("read-only");
    assert_eq!(approved.len(), 3);
    assert_eq!(approved_mcp_additions("writable").len(), 6);
    let mut served = golden[1].clone();
    served["result"]["tools"]
        .as_array_mut()
        .unwrap()
        .extend(approved.iter().cloned());
    assert_eq!(
        take_mcp_additions(&mut served, &approved),
        Some(approved.clone())
    );
    assert_eq!(
        served, golden[1],
        "removing the additions restores the baseline"
    );
    let mut drift = golden[1].clone();
    let mut changed = approved.clone();
    changed[0]["description"] = Value::String("changed".to_string());
    drift["result"]["tools"]
        .as_array_mut()
        .unwrap()
        .extend(changed);
    assert_ne!(
        take_mcp_additions(&mut drift, &approved),
        Some(approved.clone())
    );
    let mut extra = golden[1].clone();
    extra["result"]["tools"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"name": "vlite_unapproved"}));
    assert_eq!(take_mcp_additions(&mut extra, &approved), Some(Vec::new()));
    assert_ne!(extra, golden[1], "an unapproved tool is never removed");
    let mut other = golden[0].clone();
    assert_eq!(take_mcp_additions(&mut other, &approved), None);
}

fn mcp_transcript(mode: &str, graph_enabled: bool) -> Vec<Value> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp.vlite");
    let format = if graph_enabled {
        vlite::FormatVersion::V5
    } else {
        // The baseline transcript is of a vector-only file.
        vlite::FormatVersion::V3
    };
    let mut db = Database::create_with_format(&path, 2, format).unwrap();
    db.put(1, &[0.0, 0.0], b"origin").unwrap();
    db.put(2, &[1.0, 1.0], b"near").unwrap();
    db.put(3, &[2.0, 2.0], b"far").unwrap();
    db.close().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_vlite-mcp"))
        .args(["--db", path.to_str().unwrap(), &format!("--{mode}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(fixture("mcp_requests.jsonl").as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn reg008_mcp_tools_list_and_outputs_match_baseline() {
    for (mode, golden) in [
        ("read-only", "mcp_read_only.json"),
        ("writable", "mcp_writable.json"),
    ] {
        let expected: Vec<Value> = serde_json::from_str(&fixture(golden)).unwrap();
        let actual = mcp_transcript(mode, false);
        let approved = approved_mcp_additions(mode);
        assert_eq!(actual.len(), expected.len(), "{mode}: response count");
        for (actual, expected) in actual.iter().zip(&expected) {
            let (mut actual, mut expected) = (actual.clone(), expected.clone());
            let served_version = mask_mcp_version(&mut actual);
            let golden_version = mask_mcp_version(&mut expected);
            if let Some(baseline) = expected.pointer("/result/tools") {
                let mut ordered = baseline.as_array().unwrap().clone();
                ordered.extend(approved.iter().cloned());
                assert_eq!(
                    actual["result"]["tools"],
                    serde_json::json!(ordered),
                    "{mode}: baseline tools followed by approved KV tools"
                );
            }
            if let Some(taken) = take_mcp_additions(&mut actual, &approved) {
                assert_eq!(taken, approved, "{mode}: approved KV tools");
            }
            assert_eq!(
                served_version.is_some(),
                golden_version.is_some(),
                "{mode}: MCP response id {} serverInfo.version presence",
                expected["id"]
            );
            if let Some(version) = served_version {
                assert_eq!(
                    version,
                    env!("CARGO_PKG_VERSION"),
                    "{mode}: MCP server version"
                );
            }
            assert_eq!(
                actual, expected,
                "{mode}: MCP response id {} differs from baseline",
                expected["id"]
            );
        }

        let graph_actual = mcp_transcript(mode, true);
        let graph_tools = graph_actual[1]["result"]["tools"].as_array().unwrap();
        let mut ordered = expected[1]["result"]["tools"].as_array().unwrap().clone();
        ordered.extend(approved.iter().cloned());
        assert_eq!(
            graph_tools.get(..ordered.len()),
            Some(ordered.as_slice()),
            "{mode}: graph-enabled files keep baseline tools then approved KV tools"
        );
        assert_eq!(
            graph_tools[ordered.len()]["name"],
            "vlite_graph_list",
            "{mode}: graph tools follow the approved KV tools"
        );
    }
}
