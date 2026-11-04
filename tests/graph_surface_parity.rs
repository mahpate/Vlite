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
        match remainder.matches(addition).count() {
            0 => {}
            1 => {
                let start = remainder.find(addition).unwrap();
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
/// mode and all four with writes enabled. Their exact definitions are pinned
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
    assert_eq!(approved.len(), 2);
    assert_eq!(approved_mcp_additions("writable").len(), 4);
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

fn mcp_transcript(mode: &str) -> Vec<Value> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp.vlite");
    // The baseline transcript is of a vector-only file.
    let mut db = Database::create_with_format(&path, 2, vlite::FormatVersion::V3).unwrap();
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
        let actual = mcp_transcript(mode);
        let approved = approved_mcp_additions(mode);
        assert_eq!(actual.len(), expected.len(), "{mode}: response count");
        for (actual, expected) in actual.iter().zip(&expected) {
            let (mut actual, mut expected) = (actual.clone(), expected.clone());
            let served_version = mask_mcp_version(&mut actual);
            let golden_version = mask_mcp_version(&mut expected);
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
    }
}
