"""Synchronous VLite REST client. No retries or redirect following."""

import base64
import binascii
import json
from http.client import HTTPException
import math
from os import PathLike
import re
from typing import Literal, NotRequired, TypedDict
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlencode, urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener


class StringValue(TypedDict):
    type: Literal["string"]
    value: str


class IntegerValue(TypedDict):
    type: Literal["integer"]
    value: str


class FloatValue(TypedDict):
    type: Literal["float"]
    value: float


class BooleanValue(TypedDict):
    type: Literal["boolean"]
    value: bool


MetadataValue = StringValue | IntegerValue | FloatValue | BooleanValue
Metadata = dict[str, MetadataValue]


class Put(TypedDict):
    vector: list[float]
    payload_base64: NotRequired[str]
    metadata: NotRequired[Metadata]
    expires_at_ms: NotRequired[int | None]
    ttl_ms: NotRequired[int]
    ttl: NotRequired[int]


class Record(TypedDict):
    id: str
    vector: list[float]
    payload_base64: str
    metadata: Metadata
    # Per-record revision (decimal u64 string); absent on older servers.
    revision: NotRequired[str]


class VersionedRecord(TypedDict):
    record: Record
    etag: str


class Summary(TypedDict):
    id: str
    metadata: Metadata


class Page(TypedDict):
    items: list[Summary]
    next_after: str | None


class Revision(TypedDict):
    revision: int


Expected = Literal["any", "absent"] | Revision


class KvEntry(TypedDict):
    key: str
    value: bytes
    version: int
    expires_at_ms: int | None


class KvPutResult(TypedDict):
    version: int
    expires_at_ms: int | None


class KvPage(TypedDict):
    items: list[KvEntry]
    next_after: str | None


class Equal(TypedDict):
    op: Literal["eq"]
    field: str
    value: MetadataValue


class Range(TypedDict):
    op: Literal["range"]
    field: str
    min: NotRequired[IntegerValue | FloatValue | None]
    max: NotRequired[IntegerValue | FloatValue | None]


class Fusion(TypedDict, total=False):
    """Hybrid RRF options: rrf_k 1..1000 (60), weights finite 0..10 not both 0
    (1.0), candidates k..1000 (max(k, 50))."""

    rrf_k: int
    vector_weight: float
    text_weight: float
    candidates: int


# Functional syntax preserves the wire key ``lambda`` (a Python keyword).
# Custom mode requires provider; lambda is valid only for MMR. The server
# validates these mode-dependent constraints and candidate bounds.
RerankOptions = TypedDict("RerankOptions", {
    "mode": Literal["exact", "mmr", "custom"],
    "candidates": NotRequired[int],
    "lambda": NotRequired[float],
    "provider": NotRequired[str],
})


class EmbedResponse(TypedDict):
    vector: list[float]
    dimensions: int
    provider: str


class Search(TypedDict):
    """One request shape for every mode. Approximate modes support squared_l2 and
    cosine only. ``filter`` is an AND of typed conditions (None/[] keeps every
    record), valid in exact mode and, on servers advertising
    ``features.filtered_ann``, in both approximate modes; those return at most k
    eligible records and may return fewer. ``ef_search`` (approximate only) then
    controls eligible-candidate retention, not a visit/time cap.

    ``auto`` lets the server choose exact (scan or metadata-index candidates) or a
    *ready* ANN index; it never builds. ``hybrid`` fuses BM25 over declared text
    fields with a vector leg (``exact`` or ``approximate_ready``, never a lazy
    build); ``vector`` may be omitted only in hybrid mode. ``explain`` adds
    ``plan``/``stats`` to the response (use ``search_detailed``)."""

    vector: NotRequired[list[float]]
    k: int
    metric: NotRequired[Literal["squared_l2", "cosine", "inner_product"]]
    mode: NotRequired[Literal["exact", "approximate", "approximate_ready", "auto", "hybrid"]]
    ef_search: NotRequired[int]
    filter: NotRequired[list[Equal | Range] | None]
    text: NotRequired[str]
    fusion: NotRequired[Fusion]
    vector_leg: NotRequired[Literal["exact", "approximate_ready"]]
    quantization: NotRequired[Literal["none", "scalar_u8"]]
    rerank_multiplier: NotRequired[int]
    rerank: NotRequired[RerankOptions]
    config: NotRequired["IndexConfig"]
    explain: NotRequired[bool]


class BatchPut(Put):
    op: Literal["put"]
    id: str


class BatchDelete(TypedDict):
    op: Literal["delete"]
    id: str


class Hit(TypedDict):
    id: str
    distance: float
    rerank_score: NotRequired[float]


class HybridHit(TypedDict):
    """Ordered by score descending, then numeric ID."""

    id: str
    score: float
    distance: float | None
    bm25: float | None
    vector_rank: int | None
    text_rank: int | None
    rerank_score: NotRequired[float]


class QueryPlan(TypedDict):
    strategy: Literal["exact_scan", "exact_indexed", "filtered_ann", "ann"]
    estimated_candidates: int | str | None
    live_records: int | str
    index_field: str | None
    reason: str


class SearchStats(TypedDict):
    records_scanned: int
    distance_evaluations: int
    quantized_distance_evaluations: int
    graph_nodes_visited: int
    buffered_records_merged: int
    reranked_candidates: int
    index_candidates: int


class Ranking(TypedDict):
    """Custom scores descend, with numeric ID ascending for ties."""

    provider: str
    order: Literal["score_desc_id_asc"]


class SearchResponse(TypedDict):
    hits: list[Hit] | list[HybridHit]
    ranking: NotRequired[Ranking]
    # Present only with explain; plan is None outside auto mode.
    plan: NotRequired[QueryPlan | None]
    stats: NotRequired[SearchStats]


class MetadataIndex(TypedDict):
    field: str
    kind: Literal["eq", "range", "eq_range"]


class PlannerPolicy(TypedDict, total=False):
    exact_max_candidates: int
    exact_max_selectivity_ppm: int


class HnswSettings(TypedDict, total=False):
    m: int
    ef_construction: int
    seed: str


class CollectionSettings(TypedDict, total=False):
    """Per-collection settings. In requests every field is optional (a settings
    update keeps omitted fields); responses carry all fields."""

    metric: Literal["squared_l2", "cosine", "inner_product"]
    hnsw: HnswSettings
    quantization: Literal["none", "scalar_u8"]
    rerank_multiplier: int
    planner: PlannerPolicy
    metadata_indexes: list[MetadataIndex]
    text_fields: list[str]


class CollectionInfo(TypedDict):
    id: int
    name: str
    dimensions: int
    live_records: int
    settings: CollectionSettings
    settings_persisted: bool


class TextIndexStatus(TypedDict):
    declared_fields: int
    available: bool
    documents: int
    terms: int
    postings: int


class CollectionDetail(CollectionInfo):
    text_index: TextIndexStatus


class CollectionList(TypedDict):
    items: list[CollectionInfo]
    format_version: int


class SettingsUpdate(TypedDict):
    collection: CollectionInfo
    persisted: bool


class AnnMemory(TypedDict):
    """Component byte counts only; this is not total process memory."""

    graph_nodes: int
    graph_link_bytes: int
    code_bytes: int
    codebook_bytes: int
    buffer_vector_bytes: int
    resident_vector_bytes: int


class CollectionIndexStatus(TypedDict):
    """Superset of the legacy IndexStatus (also returned by newer servers on
    the legacy route). ``settings_ready``: the built graph matches the
    collection's current settings."""

    ready: bool
    config: dict
    metric: Literal["squared_l2", "cosine"] | None
    quantization: Literal["none", "scalar_u8"]
    rerank_multiplier: int
    buffered_ids: int
    buffer_vector_bytes: int
    graph_nodes: int
    stale_nodes: int
    memory: AnnMemory | None
    max_buffer_ids: int
    max_buffer_vector_bytes: int
    settings_ready: bool


class QualityOptions(TypedDict, total=False):
    config: dict
    metric: Literal["squared_l2", "cosine"]
    quantization: Literal["none", "scalar_u8"]
    rerank_multiplier: int
    samples: int
    k: int
    ef_search: int
    seed: str
    max_distance_evaluations: int


class AnnQuality(TypedDict):
    recall_at_k: float | None
    samples_used: int
    distance_evaluations: int
    skipped_reason: Literal["budget", "empty"] | None


ImportPolicy = Literal["reject_chunk", "skip_invalid"]


class InvalidRow(TypedDict):
    row: int
    code: str
    message: str


class ImportChunkReport(TypedDict):
    """``outcome`` is explicit: ``committed`` or ``already_committed`` (an
    identical resubmission of the last chunk; nothing written). A rejected chunk
    raises ImportRejectedError whose ``report`` has outcome ``rejected``."""

    outcome: Literal["committed", "already_committed", "rejected"]
    job: str
    chunk: int | str
    rows_committed: int
    rows_skipped: int
    invalid_rows: int
    invalid: list[InvalidRow]
    revision: str | None
    status: NotRequired["ImportStatus"]


class ImportStatus(TypedDict):
    job: str
    collection: str
    collection_dropped: bool
    state: Literal["open", "finished", "aborted"]
    last_chunk: int | str
    rows_committed: int | str
    rows_skipped: int | str
    last_digest: str | None
    # False while a reserved job has no committed chunk (it would not survive a restart).
    durable: bool


Operation = Literal["read", "search", "search_build", "write", "import", "index",
                    "collections", "diagnostics", "backup"]


class Credential(TypedDict):
    """Scoped credential description. Secrets and hashes are never returned.
    Scopes are bound to collection identity and database lineage: a recreated
    collection or another database fails closed (403 forbidden_scope)."""

    id: str
    name: str
    collections: list[str]
    operations: list[Operation]
    lineage: str | None
    created_unix_ms: int
    expires_unix_ms: int
    revoked_unix_ms: int | None
    rotated_from: str | None
    state: Literal["active", "expired", "revoked"]


class NewCredential(TypedDict):
    name: str  # 1..64 characters
    collections: list[str]  # ["*"] or collection names
    operations: list[Operation]  # nonempty
    expires_unix_ms: int  # mandatory, at most now + 365 days


class IssuedCredential(TypedDict):
    credential: Credential
    token: str  # returned once; never log or store in plaintext
    previous: NotRequired[Credential]


class DatabaseInfo(TypedDict):
    dimensions: int
    format_version: int
    records: int


class IndexConfig(TypedDict):
    m: int
    ef_construction: int
    seed: int


class IndexStatus(TypedDict):
    metric: NotRequired[Literal["squared_l2", "cosine"] | None]
    ready: bool
    buffered_ids: int
    buffer_vector_bytes: int
    max_buffer_ids: int
    max_buffer_vector_bytes: int
    config: IndexConfig


# Agent memory (Gap 1). Names and required presence follow section 8 of the serving
# memory contract. MemoryU64/MemoryI64 values in responses are canonical decimal
# strings. Requests accept the same string or an exact in-range int (the *Input
# forms) and send the canonical string; components shared by requests and responses
# (MemoryRef, MemoryInterval, MemoryScalar, MemoryTarget) are typed with the input
# forms but always carry strings in responses.
MemoryKey = str
MemoryBinding = str
MemoryU64 = str
MemoryI64 = str
MemoryU64Input = str | int
MemoryI64Input = str | int
MemoryKind = Literal["entity", "episodic", "fact", "relation"]
MemoryOperation = Literal["remember", "correct_facts", "consolidate", "forget", "resume_erasure"]


class MemoryScope(TypedDict):
    agent: MemoryKey
    user: NotRequired[MemoryKey]  # required when session is given
    session: NotRequired[MemoryKey]


class MemoryEffectiveScope(TypedDict):
    tenant: MemoryKey  # derived by the server, never supplied
    agent: MemoryKey
    user: NotRequired[MemoryKey]
    session: NotRequired[MemoryKey]


class MemoryExpectedAbsent(TypedDict):
    absent: Literal[True]


class MemoryExpectedRevision(TypedDict):
    revision: MemoryU64Input


MemoryExpected = MemoryExpectedAbsent | MemoryExpectedRevision


class MemoryInterval(TypedDict):
    """Half-open validity window; null endpoints are unbounded and always sent."""

    valid_from_ms: MemoryI64Input | None
    valid_to_ms: MemoryI64Input | None


class MemoryRef(TypedDict):
    id: MemoryKey
    revision: MemoryU64Input  # 0 names an item created in the same bundle


class MemorySlot(TypedDict):
    subject_id: MemoryKey
    predicate: MemoryKey
    slot_key: MemoryKey


class MemorySource(TypedDict):
    source_id: MemoryKey
    source_kind: MemoryKey
    uri: NotRequired[str]  # inert: never fetched
    transformation_id: NotRequired[MemoryKey]


class MemoryProvenance(TypedDict):
    source_id: MemoryKey
    source_kind: MemoryKey
    uri: NotRequired[str]
    transformation_id: NotRequired[MemoryKey]
    content_digest: str


class MemoryStringScalar(TypedDict):
    type: Literal["string"]
    value: str


class MemoryIntegerScalar(TypedDict):
    type: Literal["integer"]
    value: MemoryI64Input


class MemoryFloatScalar(TypedDict):
    type: Literal["float"]
    value: float


class MemoryBooleanScalar(TypedDict):
    type: Literal["boolean"]
    value: bool


MemoryScalar = MemoryStringScalar | MemoryIntegerScalar | MemoryFloatScalar | MemoryBooleanScalar
MemoryProperties = dict[str, MemoryScalar]


class MemoryCondition(TypedDict):
    field: MemoryKey
    op: Literal["eq", "lt", "lte", "gt", "gte", "exists"]
    value: NotRequired[MemoryScalar]  # forbidden for exists, required otherwise


class MemoryFilter(TypedDict, total=False):
    """AND of ``all`` and OR of ``any``; at most 64 conditions in all."""

    all: list[MemoryCondition]
    any: list[MemoryCondition]


class MemoryEvidenceRef(TypedDict):
    collection: MemoryBinding
    record: MemoryU64Input


class MemoryEvidence(TypedDict):
    collection: MemoryBinding
    record: MemoryU64Input
    vector: NotRequired[list[float]]
    text: NotRequired[str]
    payload_base64: NotRequired[str]
    metadata: NotRequired[MemoryProperties]


class MemoryAssertion(TypedDict):
    subject_id: MemoryKey
    predicate: MemoryKey
    slot_key: MemoryKey
    destination_id: MemoryKey
    interval: MemoryInterval
    slot_expected: MemoryExpected
    disagreement: NotRequired[Literal["reject", "disputed"]]


class MemoryItem(TypedDict):
    id: MemoryKey
    kind: MemoryKind
    content: str
    observed_at_ms: MemoryI64Input
    importance: NotRequired[float]
    confidence: NotRequired[float]
    expires_at_ms: NotRequired[MemoryU64Input]
    source: MemorySource
    provenance: NotRequired[list[MemorySource]]
    references: NotRequired[list[MemoryRef]]
    evidence: NotRequired[list[MemoryEvidenceRef]]
    metadata: NotRequired[MemoryProperties]
    assertion: NotRequired[MemoryAssertion]
    expected: NotRequired[MemoryExpected]
    merge: NotRequired[Literal["none", "merge_provenance"]]


class MemoryMutation(TypedDict):
    """Fields shared by every mutating request. ``deadline_ms`` is the engine
    deadline (1..5000, default 250), never the HTTP timeout."""

    scope: MemoryScope
    idempotency_key: MemoryKey
    max_references: NotRequired[int]
    deadline_ms: NotRequired[int]
    require_complete_feed: NotRequired[bool]


class MemoryRememberRequest(MemoryMutation):
    items: list[MemoryItem]
    evidence: NotRequired[list[MemoryEvidence]]


MemoryRanking = TypedDict("MemoryRanking", {
    "selection": NotRequired[Literal["weighted", "exact", "mmr"]],
    "weights": NotRequired[list[float]],
    "half_life_ms": NotRequired[float],
    "future_observations": NotRequired[Literal["reject", "clamp"]],
    "lambda": NotRequired[float],
})


class MemoryLimits(TypedDict, total=False):
    candidates: int
    vector_components: int
    evidence_bytes: int
    mmr_components: int
    deadline_ms: int
    max_hops: int
    max_visited_nodes: int
    max_edges_examined: int
    max_edges_per_node: int
    max_output_nodes: int
    max_output_edges: int
    max_output_paths: int


class MemoryProjection(TypedDict, total=False):
    snippets: bool
    provenance: bool
    paths: bool
    metadata: list[MemoryKey]


class MemoryRecallRequest(TypedDict):
    scope: MemoryScope
    scopes: NotRequired[list[MemoryScope]]
    vector: NotRequired[list[float]]
    text: NotRequired[str]
    embedding_provider: NotRequired[MemoryKey]
    entity_seeds: NotRequired[list[MemoryKey]]
    kinds: NotRequired[list[MemoryKind]]
    filter: NotRequired[MemoryFilter]
    as_of_ms: NotRequired[MemoryI64Input]
    ranking: NotRequired[MemoryRanking]
    limits: NotRequired[MemoryLimits]
    projection: NotRequired[MemoryProjection]
    mode: NotRequired[Literal["exact", "approximate"]]
    limit: NotRequired[int]
    allow_partial: NotRequired[bool]


class MemoryFactsRequest(TypedDict):
    scope: MemoryScope
    slot: MemorySlot
    as_of_ms: NotRequired[MemoryI64Input]
    history: NotRequired[bool]
    limit: NotRequired[int]
    limits: NotRequired[MemoryLimits]
    projection: NotRequired[MemoryProjection]
    allow_partial: NotRequired[bool]


class MemoryCorrectionRequest(MemoryMutation):
    slot: MemorySlot
    series: MemoryRef
    slot_revision: MemoryU64Input
    interval: MemoryInterval
    action: Literal["close", "replace", "remove"]
    at_ms: NotRequired[MemoryI64Input]
    correction_interval: NotRequired[MemoryInterval]
    replacement: NotRequired[MemoryItem]
    evidence: NotRequired[list[MemoryEvidence]]


class MemoryInput(TypedDict):
    id: MemoryKey
    revision: MemoryU64Input
    disposition: NotRequired[Literal["retain", "logical_delete"]]


class MemoryConsolidateRequest(MemoryMutation):
    inputs: list[MemoryInput]
    output: MemoryItem
    evidence: NotRequired[list[MemoryEvidence]]
    dry_run: NotRequired[bool]


class MemoryTargetIds(TypedDict):
    ids: list[MemoryRef]


class MemoryTargetSeries(TypedDict):
    series: MemoryRef


class MemoryTargetEntity(TypedDict):
    entity: MemoryRef


class MemoryTargetSession(TypedDict):
    session: Literal[True]


class MemoryTargetScope(TypedDict):
    scope: Literal[True]


MemoryTarget = MemoryTargetIds | MemoryTargetSeries | MemoryTargetEntity | MemoryTargetSession | MemoryTargetScope


class MemoryForgetRequest(MemoryMutation):
    target: MemoryTarget
    mode: Literal["logical_delete", "expire"]
    expires_at_ms: NotRequired[MemoryU64Input]
    allow_job: NotRequired[bool]


class MemoryReceiptRequest(TypedDict):
    scope: MemoryScope
    operation: MemoryOperation
    idempotency_key: MemoryKey


class MemoryErasureRequest(TypedDict):
    scope: MemoryScope
    job_id: MemoryKey


class MemoryResumeRequest(MemoryMutation):
    job_id: MemoryKey
    revision: MemoryU64Input
    generation: MemoryU64Input
    resume_token: str


class MemoryReceipt(TypedDict):
    id: MemoryKey
    operation: MemoryOperation
    scope: MemoryEffectiveScope
    sequence: MemoryU64
    retry_until_ms: NotRequired[MemoryU64]
    digest_version: Literal["memory-request-v1"]


class MemoryOutcome(TypedDict):
    id: MemoryKey
    action: Literal["created", "merged", "noop", "corrected", "deleted", "expired", "retained_shared"]
    revision: MemoryU64
    expires_at_ms: NotRequired[MemoryU64]


class MemoryFeed(TypedDict):
    mode: Literal["ordinary_changes"]
    temporal_complete: Literal[False]
    ttl_complete: Literal[False]
    consumer_purge_acknowledged: Literal[False]


class MemoryErasureNotice(TypedDict):
    """Deletion is logical: never secure erasure or a backup purge."""

    live_state_only: Literal[True]
    secure_erasure: Literal[False]
    external_copies: Literal["operator_managed"]
    restore_requires_manifest: Literal[True]
    backup_history_purge: Literal["not_verified"]
    manifest_id: NotRequired[MemoryKey]


class MemoryErasure(TypedDict):
    id: MemoryKey
    scope: MemoryEffectiveScope
    target: MemoryTarget
    revision: MemoryU64
    generation: MemoryU64
    state: Literal["pending", "logical"]
    logical_visibility: Literal["hidden"]
    deleted_count: MemoryU64
    remaining_count: MemoryU64 | None  # None: unknown
    resume_token: NotRequired[str]  # absent when finished


class MemoryMutationResponse(TypedDict):
    receipt: MemoryReceipt
    replayed: bool
    outcomes: list[MemoryOutcome]
    feed: MemoryFeed
    remaining_work: MemoryU64 | None  # None: unknown
    state: NotRequired[Literal["logical", "pending", "retention_set"]]
    erasure: NotRequired[MemoryErasure]
    erasure_notice: NotRequired[MemoryErasureNotice]


class MemoryReceiptResponse(TypedDict):
    result: MemoryMutationResponse  # replayed is true


class MemoryErasureResponse(TypedDict):
    erasure: MemoryErasure
    erasure_notice: MemoryErasureNotice
    feed: MemoryFeed


class MemoryCompeting(TypedDict):
    id: MemoryKey
    interval: MemoryInterval
    revision: MemoryU64


class MemoryErrorDetail(TypedDict, total=False):
    """One consolidation-plan conflict."""

    field: MemoryKey
    limit: MemoryU64
    requested: MemoryU64
    competing: list[MemoryCompeting]
    receipt_lookup_required: bool
    code: str


class MemoryPlanResponse(TypedDict):
    dry_run: Literal[True]
    user_operations: MemoryU64
    encoded_bytes: MemoryU64
    conflicts: list[MemoryErrorDetail]
    remaining_work: MemoryU64 | None


MemoryConsolidateResponse = MemoryMutationResponse | MemoryPlanResponse


class MemoryEvidenceView(TypedDict):
    collection: MemoryBinding
    record: MemoryU64
    snippet: NotRequired[str]


class MemoryPath(TypedDict):
    nodes: list[MemoryU64]
    edges: list[MemoryU64]


class MemoryScores(TypedDict):
    """R, D, I and S are policy-versioned diagnostics: never compare them across
    ranking versions or read a fused or BM25 value as cosine similarity."""

    R: float
    D: float | None
    I: float
    S: float
    dense_rank: NotRequired[int]
    sparse_rank: NotRequired[int]
    text_rank: NotRequired[int]
    distance: NotRequired[float]
    bm25: NotRequired[float]
    sparse_score: NotRequired[float]
    fused_rank: NotRequired[int]
    fused_score: NotRequired[float]


class MemoryView(TypedDict):
    id: MemoryKey
    revision: MemoryU64
    scope: MemoryEffectiveScope
    kind: MemoryKind
    content: str
    observed_at_ms: MemoryI64
    created_at_ms: MemoryU64
    updated_at_ms: MemoryU64
    importance: float
    confidence: NotRequired[float]
    expires_at_ms: NotRequired[MemoryU64]
    status: Literal["accepted", "disputed"]
    stale: bool
    segment: MemoryInterval | None
    evidence: list[MemoryEvidenceView]
    scores: MemoryScores
    provenance: NotRequired[list[MemoryProvenance]]
    references: NotRequired[list[MemoryRef]]
    paths: NotRequired[list[MemoryPath]]
    metadata: NotRequired[MemoryProperties]


class MemoryReadMeta(TypedDict):
    """``complete`` means the declared bounded plan finished, not that the answer is
    exhaustive; ``limitations`` always lists retained_state_only."""

    scopes: list[MemoryEffectiveScope]
    as_of_ms: MemoryI64
    ttl_clock_ms: MemoryU64
    sequence: MemoryU64
    complete: bool
    truncation: list[Literal["candidates", "source_scan", "vector_components", "evidence_bytes", "mmr_components",
                             "visited_nodes", "edges", "degree", "deadline", "output", "deletion_barrier"]]
    limitations: list[Literal["historical_candidate_discovery_not_exhaustive", "retained_state_only"]]
    mode: Literal["exact_enumeration", "approximate", "bounded_expansion"]
    candidates_examined: MemoryU64
    edges_charged: MemoryU64
    vector_components: MemoryU64
    mmr_components: MemoryU64
    evidence_bytes: MemoryU64


class MemoryRecallResponse(TypedDict):
    memories: list[MemoryView]
    meta: MemoryReadMeta
    ranking_version: Literal["memory-ranking-v1"]
    selection: Literal["weighted", "exact", "mmr"]


class MemoryFactView(TypedDict):
    series: MemoryRef
    slot: MemorySlot
    slot_revision: MemoryU64
    destination_id: MemoryKey
    interval: MemoryInterval
    status: Literal["accepted", "disputed"]
    stale: bool
    evidence: list[MemoryEvidenceView]
    provenance: NotRequired[list[MemoryProvenance]]


class MemoryFactsResponse(TypedDict):
    facts: list[MemoryFactView]
    meta: MemoryReadMeta


class MemoryErrorContext(TypedDict, total=False):
    """Structured error context (``VLiteError.details``). ``feature`` names the
    capability behind a 409 memory_capability_unavailable."""

    field: str
    limit: MemoryU64
    requested: MemoryU64
    feature: str
    category: str


class VLiteError(Exception):
    """HTTP failure; status, server code and request ID are available. ``details`` is
    the optional structured error context (``field``, ``limit``, ``requested``,
    ``feature``, ``category``; unknown keys are kept), or None."""

    def __init__(self, status: int, code: str, message: str, request_id: str | None,
                 details: MemoryErrorContext | None = None):
        self.status, self.code, self.request_id, self.details = status, code, request_id, details
        super().__init__(f"HTTP {status}: {code}: {message}")


class TransportError(Exception):
    """Network/timeout/invalid response. A mutation may already be committed."""


class UnsupportedError(Exception):
    """The server lacks the requested feature; no fallback is performed."""


class ImportRejectedError(VLiteError):
    """HTTP 422 import_rows_invalid: the chunk had invalid rows under the
    reject_chunk policy. Nothing was written and the job did not advance;
    ``report`` lists the invalid rows (outcome ``rejected``)."""

    def __init__(self, status: int, code: str, message: str, request_id: str | None, report: ImportChunkReport):
        super().__init__(status, code, message, request_id)
        self.report = report


class _NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def _id(value: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"0|[1-9][0-9]{0,19}", value) or int(value) > 18446744073709551615:
        raise ValueError("ID must be a canonical decimal u64 string")
    return value


# One strong RFC 9110 entity tag of visible ASCII: no W/ prefix, wildcard,
# whitespace, control characters, inner quotes or comma-separated lists.
_ETAG = re.compile(r'"[\x21\x23-\x7e]*"')


def _etag(value: str) -> str:
    if not isinstance(value, str) or not _ETAG.fullmatch(value):
        raise ValueError("if_match must be one exact strong entity tag returned by get_versioned")
    return value


def _collection(value: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"[a-z][a-z0-9_-]{0,62}", value):
        raise ValueError("collection name must match ^[a-z][a-z0-9_-]{0,62}$")
    return value


def _credential(value: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"[a-z2-7]{12}", value):
        raise ValueError("credential ID must be 12 characters of [a-z2-7]")
    return value


def _backup(value: str) -> str:
    # Generated snapshot names only; never a path, traversal or URL syntax.
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9.-]{0,95}", value) or ".." in value:
        raise ValueError("backup name must be a generated snapshot name")
    return value


def _int(value, low: int, high: int, name: str) -> int:
    if type(value) is not int or not low <= value <= high:
        raise ValueError(f"{name} must be an integer in {low}..{high}")
    return value


def _object(value, name: str) -> dict:
    if not isinstance(value, dict):
        raise ValueError(f"{name} must be an object")
    return value


def _kv_text(value: str, name: str, maximum: int, minimum: int = 1) -> str:
    if not isinstance(value, str):
        raise ValueError(f"{name} must be a string")
    try:
        size = len(value.encode("utf-8"))
    except UnicodeEncodeError as error:
        raise ValueError(f"{name} must contain Unicode scalar values") from error
    if not minimum <= size <= maximum:
        raise ValueError(f"{name} must be {minimum}..{maximum} UTF-8 bytes")
    return value


def _kv_decimal(value: int) -> str:
    return str(_int(value, 0, 18446744073709551615, "u64"))


def _kv_expected(value: Expected):
    if isinstance(value, str) and value in ("any", "absent"):
        return value
    if isinstance(value, dict) and set(value) == {"revision"}:
        return {"revision": _kv_decimal(value["revision"])}
    raise ValueError("expected must be any, absent or {revision: int}")


def _kv_read_decimal(value, *, nonzero: bool = False) -> int:
    result = int(_id(value))
    if nonzero and result == 0:
        raise ValueError("issued version must be nonzero")
    return result


def _kv_read_put(value) -> KvPutResult:
    return {"version": _kv_read_decimal(value["version"], nonzero=True),
            "expires_at_ms": None if value["expires_at_ms"] is None else _kv_read_decimal(value["expires_at_ms"])}


def _kv_read_entry(value) -> KvEntry:
    encoded = value["value_base64"]
    if not isinstance(encoded, str) or len(encoded) > 1396388:
        raise ValueError("invalid KV base64")
    decoded = base64.b64decode(encoded, validate=True)
    if len(decoded) > 1047290 or base64.b64encode(decoded).decode("ascii") != encoded:
        raise ValueError("noncanonical KV base64")
    return {"key": _kv_text(value["key"], "key", 1024), "value": decoded, **_kv_read_put(value)}


def _policy(value: str) -> str:
    if value not in ("reject_chunk", "skip_invalid"):
        raise ValueError("policy must be reject_chunk or skip_invalid")
    return value


def _ndjson_chunks(file, size: int):
    rows = []
    for line_number, line in enumerate(file, 1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except ValueError:
            raise ValueError(f"line {line_number} is not valid JSON") from None
        if not isinstance(row, dict):
            raise ValueError(f"line {line_number} must be a JSON object")
        rows.append(row)
        if len(rows) == size:
            yield rows
            rows = []
    if rows:
        yield rows


_MEMORY_U64_TEXT = re.compile(r"0|[1-9][0-9]{0,19}")
_MEMORY_I64_TEXT = re.compile(r"0|-?[1-9][0-9]{0,18}")


def _memory_binding(value: str) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"[a-z][a-z0-9_-]{0,62}", value):
        raise ValueError("binding must match ^[a-z][a-z0-9_-]{0,62}$")
    return value


# Memory requests are validated for encoding only: field names, JSON types and the
# U64/I64 forms. Unknown fields raise and are never sent; limits, conditional rules and
# capability gates stay with the server. Each encoder returns a new value (the caller's
# request is never modified) and names the offending path in its ValueError.
def _memory_decimal(value, low: int, high: int, shape, path: str) -> str:
    if type(value) is int:
        number = value
    elif isinstance(value, str) and shape.fullmatch(value):
        number = int(value)
    else:
        raise ValueError(f"{path} must be a canonical decimal string or an exact integer")
    if not low <= number <= high:
        raise ValueError(f"{path} must be in {low}..{high}")
    return str(number)


def _memory_u64(value, path: str) -> str:
    return _memory_decimal(value, 0, 18446744073709551615, _MEMORY_U64_TEXT, path)


def _memory_i64(value, path: str) -> str:
    return _memory_decimal(value, -9223372036854775808, 9223372036854775807, _MEMORY_I64_TEXT, path)


def _memory_text(value, path: str) -> str:
    if not isinstance(value, str):
        raise ValueError(f"{path} must be a string")
    try:
        value.encode("utf-8")
    except UnicodeEncodeError as error:
        raise ValueError(f"{path} must contain Unicode scalar values") from error
    return value


def _memory_bool(value, path: str) -> bool:
    if type(value) is not bool:
        raise ValueError(f"{path} must be a boolean")
    return value


def _memory_true(value, path: str) -> bool:
    if value is not True:
        raise ValueError(f"{path} must be true")
    return True


def _memory_budget(value, path: str) -> int:
    if type(value) is not int:
        raise ValueError(f"{path} must be an integer")
    return value


def _memory_number(value, path: str):
    if type(value) is int or (isinstance(value, float) and math.isfinite(value)):
        return value
    raise ValueError(f"{path} must be a finite number")


def _memory_nullable(encode):
    return lambda value, path: None if value is None else encode(value, path)


def _memory_array(encode):
    def encoder(value, path):
        if not isinstance(value, list):
            raise ValueError(f"{path} must be an array")
        return [encode(item, f"{path}[{index}]") for index, item in enumerate(value)]
    return encoder


def _memory_map(encode):
    def encoder(value, path):
        if not isinstance(value, dict):
            raise ValueError(f"{path} must be an object")
        return {_memory_text(key, f"{path} key"): encode(item, f"{path}.{key}") for key, item in value.items()}
    return encoder


def _memory_object(required: dict, optional: dict | None = None):
    fields = {**required, **(optional or {})}

    def encoder(value, path):
        if not isinstance(value, dict):
            raise ValueError(f"{path} must be an object")
        for key in value:
            if key not in fields:
                raise ValueError(f"{path}: unknown field {key!r} is never sent")
        for key in required:
            if key not in value:
                raise ValueError(f"{path}: missing required field {key!r}")
        return {key: fields[key](item, f"{path}.{key}") for key, item in value.items()}
    return encoder


def _memory_variant(variants: dict):
    def encoder(value, path):
        if not isinstance(value, dict) or len(value) != 1:
            raise ValueError(f"{path} must be an object with exactly one of {', '.join(variants)}")
        ((key, item),) = value.items()
        if key not in variants:
            raise ValueError(f"{path}: unknown variant {key!r}")
        return {key: variants[key](item, f"{path}.{key}")}
    return encoder


_MEMORY_SCALARS = {"string": _memory_text, "integer": _memory_i64, "float": _memory_number, "boolean": _memory_bool}


def _memory_scalar(value, path: str):
    if not isinstance(value, dict) or set(value) != {"type", "value"}:
        raise ValueError(f"{path} must be an object with type and value")
    kind = value["type"]
    if not isinstance(kind, str) or kind not in _MEMORY_SCALARS:
        raise ValueError(f"{path}.type must be one of {', '.join(_MEMORY_SCALARS)}")
    return {"type": kind, "value": _MEMORY_SCALARS[kind](value["value"], f"{path}.value")}


def _memory_mutation(required: dict, optional: dict | None = None):
    return _memory_object({"scope": _MEMORY_SCOPE, "idempotency_key": _memory_text, **required},
                          {"max_references": _memory_budget, "deadline_ms": _memory_budget,
                           "require_complete_feed": _memory_bool, **(optional or {})})


_MEMORY_SCOPE = _memory_object({"agent": _memory_text}, {"user": _memory_text, "session": _memory_text})
_MEMORY_SOURCE = _memory_object({"source_id": _memory_text, "source_kind": _memory_text},
                                {"uri": _memory_text, "transformation_id": _memory_text})
_MEMORY_REF = _memory_object({"id": _memory_text, "revision": _memory_u64})
_MEMORY_INTERVAL = _memory_object({"valid_from_ms": _memory_nullable(_memory_i64),
                                   "valid_to_ms": _memory_nullable(_memory_i64)})
_MEMORY_SLOT = _memory_object({"subject_id": _memory_text, "predicate": _memory_text, "slot_key": _memory_text})
_MEMORY_EXPECTED = _memory_variant({"absent": _memory_true, "revision": _memory_u64})
_MEMORY_PROPERTIES = _memory_map(_memory_scalar)
_MEMORY_EVIDENCE_REF = _memory_object({"collection": _memory_text, "record": _memory_u64})
_MEMORY_EVIDENCE = _memory_object(
    {"collection": _memory_text, "record": _memory_u64},
    {"vector": _memory_array(_memory_number), "text": _memory_text, "payload_base64": _memory_text,
     "metadata": _MEMORY_PROPERTIES})
_MEMORY_ASSERTION = _memory_object(
    {"subject_id": _memory_text, "predicate": _memory_text, "slot_key": _memory_text, "destination_id": _memory_text,
     "interval": _MEMORY_INTERVAL, "slot_expected": _MEMORY_EXPECTED}, {"disagreement": _memory_text})
_MEMORY_ITEM = _memory_object(
    {"id": _memory_text, "kind": _memory_text, "content": _memory_text, "observed_at_ms": _memory_i64,
     "source": _MEMORY_SOURCE},
    {"importance": _memory_number, "confidence": _memory_number, "expires_at_ms": _memory_u64,
     "provenance": _memory_array(_MEMORY_SOURCE), "references": _memory_array(_MEMORY_REF),
     "evidence": _memory_array(_MEMORY_EVIDENCE_REF), "metadata": _MEMORY_PROPERTIES,
     "assertion": _MEMORY_ASSERTION, "expected": _MEMORY_EXPECTED, "merge": _memory_text})
_MEMORY_RANKING = _memory_object(
    {}, {"selection": _memory_text, "weights": _memory_array(_memory_number), "half_life_ms": _memory_number,
         "future_observations": _memory_text, "lambda": _memory_number})
_MEMORY_LIMITS = _memory_object({}, dict.fromkeys(
    ("candidates", "vector_components", "evidence_bytes", "mmr_components", "deadline_ms", "max_hops",
     "max_visited_nodes", "max_edges_examined", "max_edges_per_node", "max_output_nodes", "max_output_edges",
     "max_output_paths"), _memory_budget))
_MEMORY_PROJECTION = _memory_object(
    {}, {"snippets": _memory_bool, "provenance": _memory_bool, "paths": _memory_bool,
         "metadata": _memory_array(_memory_text)})
_MEMORY_CONDITION = _memory_object({"field": _memory_text, "op": _memory_text}, {"value": _memory_scalar})
_MEMORY_FILTER = _memory_object({}, {"all": _memory_array(_MEMORY_CONDITION), "any": _memory_array(_MEMORY_CONDITION)})
_MEMORY_TARGET = _memory_variant({"ids": _memory_array(_MEMORY_REF), "series": _MEMORY_REF, "entity": _MEMORY_REF,
                                  "session": _memory_true, "scope": _memory_true})
_MEMORY_REMEMBER = _memory_mutation({"items": _memory_array(_MEMORY_ITEM)},
                                    {"evidence": _memory_array(_MEMORY_EVIDENCE)})
_MEMORY_RECALL = _memory_object(
    {"scope": _MEMORY_SCOPE},
    {"scopes": _memory_array(_MEMORY_SCOPE), "vector": _memory_array(_memory_number), "text": _memory_text,
     "embedding_provider": _memory_text, "entity_seeds": _memory_array(_memory_text),
     "kinds": _memory_array(_memory_text), "filter": _MEMORY_FILTER, "as_of_ms": _memory_i64,
     "ranking": _MEMORY_RANKING, "limits": _MEMORY_LIMITS, "projection": _MEMORY_PROJECTION, "mode": _memory_text,
     "limit": _memory_budget, "allow_partial": _memory_bool})
_MEMORY_FACTS = _memory_object(
    {"scope": _MEMORY_SCOPE, "slot": _MEMORY_SLOT},
    {"as_of_ms": _memory_i64, "history": _memory_bool, "limit": _memory_budget, "limits": _MEMORY_LIMITS,
     "projection": _MEMORY_PROJECTION, "allow_partial": _memory_bool})
_MEMORY_CORRECTION = _memory_mutation(
    {"slot": _MEMORY_SLOT, "series": _MEMORY_REF, "slot_revision": _memory_u64, "interval": _MEMORY_INTERVAL,
     "action": _memory_text},
    {"at_ms": _memory_i64, "correction_interval": _MEMORY_INTERVAL, "replacement": _MEMORY_ITEM,
     "evidence": _memory_array(_MEMORY_EVIDENCE)})
_MEMORY_CONSOLIDATE = _memory_mutation(
    {"inputs": _memory_array(_memory_object({"id": _memory_text, "revision": _memory_u64},
                                            {"disposition": _memory_text})), "output": _MEMORY_ITEM},
    {"evidence": _memory_array(_MEMORY_EVIDENCE), "dry_run": _memory_bool})
_MEMORY_FORGET = _memory_mutation({"target": _MEMORY_TARGET, "mode": _memory_text},
                                  {"expires_at_ms": _memory_u64, "allow_job": _memory_bool})
_MEMORY_RECEIPT = _memory_object({"scope": _MEMORY_SCOPE, "operation": _memory_text, "idempotency_key": _memory_text})
_MEMORY_ERASURE = _memory_object({"scope": _MEMORY_SCOPE, "job_id": _memory_text})
_MEMORY_RESUME = _memory_mutation({"job_id": _memory_text, "revision": _memory_u64, "generation": _memory_u64,
                                   "resume_token": _memory_text})


class _Records:
    """Record, batch, search and index calls under one path prefix: ``/v1`` (the
    legacy routes, which act on the ``default`` collection with historical
    defaults) or ``/v1/collections/{name}`` (omitted metric/config/quantization
    default to that collection's settings)."""

    _prefix: str

    def _transport(self) -> "Client":
        raise NotImplementedError

    def get(self, id: str) -> Record:
        return self._transport()._request("GET", self._prefix + "/vectors/" + _id(id))

    def get_versioned(self, id: str) -> VersionedRecord:
        """Return {"record": ..., "etag": ...}. The tag is opaque; pass it back
        unchanged. Servers advertising ``features.per_record_etags`` invalidate it
        only when this record changes (and, for v1-v3 files, on restart); older
        servers invalidate it on any committed write or restart. A missing/invalid
        ETag (for example, an older server) raises TransportError."""
        record, tag = self._transport()._request("GET", self._prefix + "/vectors/" + _id(id), etag=True)
        return {"record": record, "etag": tag}

    def put(self, id: str, record: Put, *, if_match: str | None = None) -> None:
        """Replace a record. With if_match, send it unchanged as If-Match; a stale
        tag raises VLiteError 412 precondition_failed and nothing is retried. A
        conditional write first checks /v1/capabilities and raises UnsupportedError
        on servers that do not advertise conditional_writes."""
        path = self._prefix + "/vectors/" + _id(id)
        client = self._transport()
        client._request("PUT", path, record, if_match=client._conditional(if_match))

    def delete(self, id: str, *, if_match: str | None = None) -> bool:
        """Delete a record. With if_match, a stale tag or missing record raises
        VLiteError 412 precondition_failed and nothing is retried. Conditional deletes
        check capabilities first, like put."""
        path = self._prefix + "/vectors/" + _id(id)
        client = self._transport()
        return client._request("DELETE", path, if_match=client._conditional(if_match))["deleted"]

    def list_vectors(self, *, limit: int = 25, after: str | None = None) -> Page:
        if type(limit) is not int or not 1 <= limit <= 100:
            raise ValueError("limit must be an integer in 1..100")
        query = {"limit": str(limit)}
        if after is not None:
            query["after"] = _id(after)
        return self._transport()._request("GET", self._prefix + "/vectors?" + urlencode(query))

    def batch(self, operations: list[BatchPut | BatchDelete]) -> None:
        for operation in operations:
            _id(operation["id"])
        self._transport()._request("POST", self._prefix + "/batches", {"operations": operations})

    def search(self, request: Search) -> list[Hit] | list[HybridHit]:
        """Send the request unchanged and return the hits. There is no exact
        fallback: an older server's 422 (approximate filter, unknown mode/field) or
        a 409 from a ready-only mode is raised. Hybrid mode returns HybridHit
        entries ordered by score descending; custom reranking orders by
        rerank_score descending then numeric ID, preserving metric distance.
        Use search_detailed to retain ranking metadata."""
        return self.search_detailed(request)["hits"]

    def search_detailed(self, request: Search) -> SearchResponse:
        """Like search, but return the whole response. With ``explain: True`` it
        carries ``stats`` and ``plan`` (``plan`` is null outside ``auto`` mode).
        Custom reranking may add ``ranking``; older responses may omit it."""
        return self._transport()._request("POST", self._prefix + "/search", request)

    def _feature_prefix(self) -> str:
        return "/v1/collections/default" if self._prefix == "/v1" else self._prefix

    def embed_text(self, text: str, *, provider: str = "hashing") -> EmbedResponse:
        """Embed using hashing, legacy http or an admin-authorized alias.
        Provider failures propagate without retry or fallback."""
        if not isinstance(text, str) or len(text.encode("utf-8")) > 16384:
            raise ValueError("text must be at most 16384 UTF-8 bytes")
        if not isinstance(provider, str) or not re.fullmatch(r"[A-Za-z0-9._-]{1,256}", provider):
            raise ValueError("provider must be 1..256 ASCII letters, digits, dot, underscore or hyphen")
        return self._transport()._request(
            "POST", self._feature_prefix() + "/embed", {"text": text, "provider": provider}
        )

    def update_metadata(self, id: str, metadata: Metadata, *, if_match: str | None = None) -> None:
        """Replace metadata without rewriting vector or payload; requires V5."""
        if not isinstance(metadata, dict):
            raise ValueError("metadata must be an object")
        self._transport()._request("PATCH", self._feature_prefix() + "/vectors/" + _id(id) + "/metadata",
                                   {"metadata": metadata}, if_match=self._transport()._conditional(if_match))

    def grouped_search(self, request: dict) -> dict:
        return self._transport()._request("POST", self._feature_prefix() + "/search/grouped", _object(request, "request"))

    def recommend(self, request: dict) -> dict:
        return self._transport()._request("POST", self._feature_prefix() + "/recommend", _object(request, "request"))

    def facets(self, field: str, *, filter: list | None = None) -> dict:
        return self._transport()._request("POST", self._feature_prefix() + "/facets", {"field": field, "filter": filter or []})

    def numeric_stats(self, field: str, *, filter: list | None = None) -> dict:
        return self._transport()._request("POST", self._feature_prefix() + "/stats/numeric", {"field": field, "filter": filter or []})

    def geo_search(self, request: dict) -> dict:
        return self._transport()._request("POST", self._feature_prefix() + "/search/geo", _object(request, "request"))

    def configure_vector(self, name: str, *, dimensions: int, metric: str = "squared_l2", active_version: str = "v1", history: int = 0) -> dict:
        return self._transport()._request("POST", self._feature_prefix() + "/vectors/configure", {"name": name, "dimensions": dimensions, "metric": metric, "active_version": active_version, "history": history})

    def put_named_vectors(self, id: str, *, dense: dict | None = None, sparse: dict | None = None) -> dict:
        return self._transport()._request("PUT", self._feature_prefix() + "/vectors/" + _id(id) + "/named", {"dense": dense or {}, "sparse": sparse or {}})

    def search_named(self, request: dict) -> dict:
        return self._transport()._request("POST", self._feature_prefix() + "/search/named", _object(request, "request"))

    def search_sparse(self, request: dict) -> dict:
        return self._transport()._request("POST", self._feature_prefix() + "/search/sparse", _object(request, "request"))

    def search_dense_sparse(self, request: dict) -> dict:
        return self._transport()._request("POST", self._feature_prefix() + "/search/dense-sparse", _object(request, "request"))

    def query(self, query: str, vectors: dict | None = None) -> dict:
        return self._transport()._request("POST", self._feature_prefix() + "/query", {"query": query, "vectors": vectors or {}})

    def build_index(self, *, metric: Literal["squared_l2", "cosine"] | None = None,
                    quantization: Literal["none", "scalar_u8"] | None = None,
                    rerank_multiplier: int | None = None) -> None:
        """Synchronously build the HNSW index. Arguments left as None are omitted:
        legacy routes then use squared_l2 without quantization, collection routes
        use the collection's settings."""
        body = self._index_body(metric, quantization, rerank_multiplier)
        self._transport()._request("POST", self._prefix + "/index/hnsw", body)

    def rebuild_status(self) -> dict:
        return self._transport()._request("GET", self._prefix + "/index/hnsw/rebuild")

    def start_rebuild(self, request: dict | None = None) -> dict:
        """Start a background rebuild (202). ``request`` is sent unchanged (for
        example ``metric``, ``config``, ``limits``, ``quantization``,
        ``rerank_multiplier``); a busy rebuild is a 409, never queued or retried."""
        body = {} if request is None else _object(request, "request")
        return self._transport()._request("POST", self._prefix + "/index/hnsw/rebuild", body)

    def poll_rebuild(self) -> dict:
        return self._transport()._request("POST", self._prefix + "/index/hnsw/rebuild/poll")

    def cancel_rebuild(self) -> dict:
        return self._transport()._request("POST", self._prefix + "/index/hnsw/rebuild/cancel")

    def maintenance_status(self) -> dict:
        """``{policy, status}`` (collection routes add ``identity``)."""
        return self._transport()._request("GET", self._prefix + "/index/hnsw/maintenance")

    def maintenance_tick(self) -> dict:
        """Run one bounded maintenance step: ``{outcome, started, status}``."""
        return self._transport()._request("POST", self._prefix + "/index/hnsw/maintenance/tick")

    def _index_body(self, metric, quantization, rerank_multiplier) -> dict:
        body: dict = {}
        if metric is not None:
            body["metric"] = metric
        if quantization is not None:
            body["quantization"] = quantization
        if rerank_multiplier is not None:
            body["rerank_multiplier"] = _int(rerank_multiplier, 1, 16, "rerank_multiplier")
        return body


class Collection(_Records):
    """Record, search and index operations scoped to one named collection.
    Obtain with ``Client.collection(name)``; it shares the client's connection.
    An unknown collection raises VLiteError 404 ``collection_not_found``; nothing
    falls back to ``default``."""

    def __init__(self, client: "Client", name: str):
        self._client, self.name = client, _collection(name)
        self._prefix = "/v1/collections/" + name

    def _transport(self) -> "Client":
        return self._client

    def info(self) -> CollectionDetail:
        return self._client.get_collection(self.name)

    def index_status(self) -> CollectionIndexStatus:
        """Readiness, identity (config/metric/quantization/rerank_multiplier),
        graph/stale-node counts and component memory bytes (``memory``, null
        without a graph; not process RSS)."""
        return self._client._request("GET", self._prefix + "/index/hnsw")

    def ann_quality(self, options: QualityOptions | None = None) -> AnnQuality:
        """Sampled recall@k of the *ready* index against exact search (409
        index_not_ready otherwise; never builds). ``skipped_reason`` is "budget"
        or "empty" when no sample ran."""
        body = {} if options is None else _object(options, "options")
        return self._client._request("POST", self._prefix + "/index/hnsw/quality", body)


class Memory:
    """The nine agent-memory operations of one provisioned binding
    (``/v1/memory/{binding}``). Obtain with ``Client.memory(binding)``: no request
    is made and the client's connection is shared. Each method takes one request
    mapping, POSTs it and returns the server's full response, including a 202
    pending result; nothing is retried, split, paginated, embedded or defaulted.

    Requests are checked for encoding only. Unknown fields raise ValueError and are
    never sent; U64/I64 inputs take a canonical decimal string or an exact in-range
    int and are sent as the canonical string. Limits, conditional rules and
    capability gates belong to the server, whose refusals are raised unchanged as
    VLiteError (``code``, ``status``, ``request_id`` and ``details``; for example 409
    memory_capability_unavailable with ``details["feature"]``). A server without
    memory routes (404 not_found) raises UnsupportedError. A mutation that ends in
    TransportError may have committed: look up a forget or consolidate with
    ``get_receipt``. The server refuses a lookup of a remember receipt (409
    memory_capability_unavailable, ``details["feature"]`` remember_receipt_lookup),
    so repeat the same remember request, with the same idempotency key and body,
    inside the receipt window.

    ``deadline_ms`` in a body is the engine deadline, not the HTTP timeout
    (``Client(timeout=...)``)."""

    def __init__(self, client: "Client", binding: str):
        self._client, self.binding = client, _memory_binding(binding)
        self._prefix = "/v1/memory/" + binding

    def _post(self, route: str, encode, request):
        return self._client._memory_request(self._prefix + route, encode(request, "request"))

    def remember(self, request: MemoryRememberRequest) -> MemoryMutationResponse:
        """Commit 1-4,096 caller-built items atomically with a receipt (kinds
        entity and episodic on the first server slice)."""
        return self._post("/remember", _MEMORY_REMEMBER, request)

    def recall(self, request: MemoryRecallRequest) -> MemoryRecallResponse:
        """Ranked bounded read of retained state; there is no pagination."""
        return self._post("/recall", _MEMORY_RECALL, request)

    def find_facts(self, request: MemoryFactsRequest) -> MemoryFactsResponse:
        """Query one fact slot (not available on the first server slice)."""
        return self._post("/facts/query", _MEMORY_FACTS, request)

    def correct_facts(self, request: MemoryCorrectionRequest) -> MemoryMutationResponse:
        """Close, replace or remove a fact segment (not available on the first server slice)."""
        return self._post("/facts/correct", _MEMORY_CORRECTION, request)

    def consolidate(self, request: MemoryConsolidateRequest) -> MemoryConsolidateResponse:
        """Commit a caller-produced summary (a MemoryMutationResponse) or, with
        ``dry_run``, return a MemoryPlanResponse; the whole response is returned."""
        return self._post("/consolidate", _MEMORY_CONSOLIDATE, request)

    def forget(self, request: MemoryForgetRequest) -> MemoryMutationResponse:
        """Logical deletion or retention expiry: HTTP 200 for a logical result and
        202 for a pending job; neither means secure erasure."""
        return self._post("/forget", _MEMORY_FORGET, request)

    def get_receipt(self, request: MemoryReceiptRequest) -> MemoryReceiptResponse:
        """Look up a retained receipt by scope, operation and idempotency key (the
        first server slice refuses operation remember: 409 remember_receipt_lookup)."""
        return self._post("/receipts/lookup", _MEMORY_RECEIPT, request)

    def get_erasure(self, request: MemoryErasureRequest) -> MemoryErasureResponse:
        """Read an erasure job's status (not available on the first server slice)."""
        return self._post("/erasures/get", _MEMORY_ERASURE, request)

    def resume_erasure(self, request: MemoryResumeRequest) -> MemoryMutationResponse:
        """Commit one bounded erasure chunk with the job's exact revision,
        generation and token (not available on the first server slice)."""
        return self._post("/erasures/resume", _MEMORY_RESUME, request)


class Client(_Records):
    _prefix = "/v1"

    def __init__(self, base_url: str, token: str, *, timeout: float = 30):
        url = urlsplit(base_url)
        if url.scheme not in ("http", "https") or not url.hostname or url.username is not None or url.password is not None or url.query or url.fragment or url.path not in ("", "/"):
            raise ValueError("base_url must be an HTTP(S) origin without credentials, path, query or fragment")
        # Bootstrap tokens and scoped vlk_<id>_<secret> credentials share this alphabet.
        if not re.fullmatch(r"[A-Za-z0-9_-]{32,256}", token):
            raise ValueError("token must contain 32..256 base64url characters")
        if not math.isfinite(timeout) or timeout <= 0:
            raise ValueError("timeout must be finite and positive")
        self._base, self._token, self._timeout = base_url.rstrip("/"), token, timeout
        # Do not send the bearer token through implicitly configured environment proxies.
        self._opener = build_opener(ProxyHandler({}), _NoRedirect())

    def _transport(self) -> "Client":
        return self

    def _request(self, method: str, path: str, body=None, *, if_match: str | None = None, etag: bool = False,
                 text: bool = False):
        data = None if body is None else json.dumps(body, allow_nan=False).encode("utf-8")
        headers = {"Authorization": "Bearer " + self._token, "Accept": "text/plain" if text else "application/json"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        if if_match is not None:
            headers["If-Match"] = if_match
        request = Request(self._base + path, data=data, headers=headers, method=method)
        try:
            with self._opener.open(request, timeout=self._timeout) as response:
                raw = response.read()
                if text:
                    if not (response.headers.get("Content-Type") or "").startswith("text/plain"):
                        raise ValueError("expected a text/plain response")
                    return raw.decode("utf-8")
                value = json.loads(raw) if raw else None
                if not etag:
                    return value
                tags = response.headers.get_all("ETag") or []
                if len(tags) != 1 or not _ETAG.fullmatch(tags[0]):
                    raise ValueError("response must carry exactly one strong ETag")
                return value, tags[0]
        except HTTPError as error:
            with error:
                raw = error.read()
                try:
                    document = json.loads(raw)
                    detail = document["error"]
                    code, message = detail["code"], detail["message"]
                    details = detail.get("details")
                except (ValueError, KeyError, TypeError):
                    document, code, message, details = None, "http_error", "Server returned a nonstandard error response", None
                request_id = error.headers.get("X-Request-ID")
                if error.code == 422 and code == "import_rows_invalid" and isinstance(document, dict):
                    report = {key: value for key, value in document.items() if key != "error"}
                    raise ImportRejectedError(error.code, code, message, request_id, dict(report, outcome="rejected")) from None
                raise VLiteError(error.code, code, message, request_id, details if isinstance(details, dict) else None) from None
        except (URLError, OSError, HTTPException, ValueError) as error:
            raise TransportError("Request failed or response was invalid; mutation outcome may be unknown; no retry performed") from error

    def health(self) -> dict[str, str]:
        return self._request("GET", "/health")

    def ready(self) -> dict[str, str]:
        return self._request("GET", "/ready")

    def database(self) -> DatabaseInfo:
        return self._request("GET", "/v1/database")

    def metrics(self) -> dict[str, object]:
        return self._request("GET", "/metrics")

    def prometheus_metrics(self) -> str:
        """GET /metrics?format=prometheus as text (text/plain; version=0.0.4).
        Any other content type raises TransportError."""
        return self._request("GET", "/metrics?format=prometheus", text=True)

    def capabilities(self) -> dict:
        """Fresh GET /v1/capabilities (never cached): role, format_version,
        ``features`` flags and, for scoped tokens, ``scope``."""
        return self._request("GET", "/v1/capabilities")

    def _require_conditional_writes(self) -> None:
        # Older servers ignore If-Match; a fresh check per write avoids silently
        # downgrading to an unconditional mutation (never cached across restarts).
        try:
            caps = self._request("GET", "/v1/capabilities")
        except VLiteError as error:
            if error.status != 404:
                raise
            caps = None
        features = caps.get("features") if isinstance(caps, dict) else None
        if not (isinstance(caps, dict) and type(caps.get("api_version")) is int and caps["api_version"] == 1
                and isinstance(features, dict) and features.get("conditional_writes") is True):
            raise UnsupportedError("Server does not advertise conditional_writes (API version 1); no write was sent")

    def _conditional(self, if_match: str | None) -> str | None:
        if if_match is None:
            return None
        _etag(if_match)
        self._require_conditional_writes()
        return if_match

    def index_status(self) -> IndexStatus:
        return self._request("GET", "/v1/index/hnsw")

    def build_index(self, *, metric: Literal["squared_l2", "cosine"] | None = "squared_l2",
                    quantization: Literal["none", "scalar_u8"] | None = None,
                    rerank_multiplier: int | None = None) -> None:
        """Synchronously build the default collection's index (legacy route)."""
        body = self._index_body(None if metric == "squared_l2" else metric, quantization, rerank_multiplier)
        self._request("POST", "/v1/index/hnsw", body)

    # Collections (v4 databases; v1-v3 return 409 requires_format_v4).

    def collection(self, name: str) -> Collection:
        """Return a handle for one collection's record/search/index routes. No
        request is made; ``default`` addresses collection 0 explicitly."""
        return Collection(self, name)

    def list_collections(self) -> CollectionList:
        return self._request("GET", "/v1/collections")

    def create_collection(self, name: str, dimensions: int, settings: CollectionSettings | None = None) -> CollectionInfo:
        """Create a collection (201). Omitted settings use server defaults.
        Errors: 409 requires_format_v4 / collection_exists / collection_limit,
        422 invalid_input."""
        body: dict = {"name": _collection(name), "dimensions": _int(dimensions, 1, 2**32 - 1, "dimensions")}
        if settings is not None:
            body["settings"] = _object(settings, "settings")
        return self._request("POST", "/v1/collections", body)

    def get_collection(self, name: str) -> CollectionDetail:
        return self._request("GET", "/v1/collections/" + _collection(name))

    def drop_collection(self, name: str) -> bool:
        """Drop an empty collection (409 collection_not_empty otherwise)."""
        return self._request("DELETE", "/v1/collections/" + _collection(name))["deleted"]

    def set_collection_settings(self, name: str, settings: CollectionSettings) -> SettingsUpdate:
        """Replace the given settings fields (omitted fields keep their values).
        ``persisted`` is False when the change is memory-only (``default`` on a
        v1-v3 file)."""
        path = "/v1/collections/" + _collection(name) + "/settings"
        return self._request("PUT", path, _object(settings, "settings"))

    # Resumable imports (v4).

    def begin_import(self, collection: str) -> ImportStatus:
        """Reserve a job. It becomes durable with its first committed chunk; a job
        without a durable chunk does not survive a restart."""
        return self._request("POST", "/v1/collections/" + _collection(collection) + "/imports")

    def import_chunk(self, job: str, chunk: str, operations: list[BatchPut | BatchDelete], *,
                     policy: ImportPolicy | None = None) -> ImportChunkReport:
        """Commit one chunk (<= 4096 operations) atomically with its checkpoint.
        Returns outcome ``committed`` or ``already_committed`` (identical resubmission
        of the last chunk; nothing written). Invalid rows under ``reject_chunk``
        raise ImportRejectedError (422; nothing written, job not advanced).
        A different payload for a committed chunk, or a gap, is 409 import_conflict."""
        body: dict = {"operations": operations}
        if policy is not None:
            body["policy"] = _policy(policy)
        return self._request("PUT", f"/v1/imports/{_id(job)}/chunks/{_id(chunk)}", body)

    def import_status(self, job: str) -> ImportStatus:
        return self._request("GET", "/v1/imports/" + _id(job))

    def finish_import(self, job: str) -> ImportStatus:
        return self._request("POST", f"/v1/imports/{_id(job)}/finish")

    def abort_import(self, job: str) -> ImportStatus:
        """Close the job; already committed chunks are not rolled back."""
        return self._request("POST", f"/v1/imports/{_id(job)}/abort")

    def import_ndjson(self, path: str | PathLike, collection: str, *, chunk_size: int = 1024,
                      policy: ImportPolicy = "reject_chunk", job: str | None = None) -> ImportStatus:
        """Import a file of batch operations (one JSON object per line; blank lines
        ignored) in chunks of ``chunk_size`` (1..4096) and finish the job.

        With ``job``, resume: the job's ``last_chunk`` is read from the server,
        earlier chunks are skipped, the last committed chunk is resubmitted and must
        be confirmed ``already_committed`` (proving the same file, chunk size and
        policy), and the rest are sent. A finished job returns its status without
        sending anything. Nothing is retried: a TransportError or VLiteError stops
        the import and the job can be resumed; ImportRejectedError reports invalid
        rows of the chunk that was not written.

        The whole file is read and validated before a job is reserved or resumed,
        so a missing, unreadable, empty or malformed file sends nothing. Any later
        failure re-raises the original exception with ``import_job`` set to the
        job ID; the job is never aborted automatically, so committed chunks stay
        resumable (pass ``job=error.import_job``) or can be aborted explicitly."""
        _collection(collection)
        _int(chunk_size, 1, 4096, "chunk_size")
        _policy(policy)
        if job is not None:
            _id(job)
        with open(path, encoding="utf-8") as file:
            chunks = sum(1 for _ in _ndjson_chunks(file, chunk_size))
        if job is None and chunks == 0:
            raise ValueError("file contains no operations")
        if job is None:
            status = self.begin_import(collection)
        else:
            status = self.import_status(job)
            if status["collection"] != collection or status.get("collection_dropped"):
                raise ValueError("import job belongs to a different collection")
            if status["state"] == "finished":
                return status
            if status["state"] != "open":
                raise ValueError("import job is not open")
        job = _id(status["job"])
        try:
            last = status["last_chunk"]
            if isinstance(last, str) and last.isascii() and last.isdigit():
                last = int(last)
            if isinstance(last, bool) or not isinstance(last, int) or last < 0:
                raise TransportError("import status has an invalid last_chunk")
            if chunks < last:
                raise ValueError("file has fewer chunks than the job already committed")
            count = 0
            with open(path, encoding="utf-8") as file:
                for count, rows in enumerate(_ndjson_chunks(file, chunk_size), 1):
                    if count < last:
                        continue
                    report = self.import_chunk(job, str(count), rows, policy=policy)
                    expected = "already_committed" if count == last else "committed"
                    if report.get("outcome") != expected:
                        raise TransportError(f"import chunk {count}: expected outcome {expected}; no retry performed")
            if count != chunks:
                raise ValueError("file changed during import")
            return self.finish_import(job)
        except Exception as error:
            error.import_job = job
            raise

    # Operations (admin).

    def _kv_request(self, method: str, path: str, body=None):
        try:
            return self._request(method, path, body)
        except VLiteError as error:
            if error.status == 404 and error.code == "not_found":
                raise UnsupportedError("Server does not support KV routes; no fallback performed") from error
            raise

    def _kv_decode(self, decoder, value):
        try:
            return decoder(value)
        except (ValueError, KeyError, TypeError, binascii.Error) as error:
            raise TransportError("Invalid KV response; no retry performed") from error

    def kv_get(self, *, collection: str, namespace: str, key: str) -> KvEntry:
        query = urlencode({"namespace": _kv_text(namespace, "namespace", 255),
                           "key": _kv_text(key, "key", 1024)}, quote_via=quote)
        value = self._kv_request("GET", "/v1/collections/" + _collection(collection) + "/kv/entry?" + query)
        return self._kv_decode(_kv_read_entry, value)

    def kv_put(self, *, collection: str, namespace: str, key: str, value: bytes,
               expected: Expected = "any", expires_at_ms: int | None = None) -> KvPutResult:
        if not isinstance(value, bytes) or len(value) > 1047290:
            raise ValueError("value must be bytes of length at most 1047290")
        body = {"namespace": _kv_text(namespace, "namespace", 255), "key": _kv_text(key, "key", 1024),
                "value_base64": base64.b64encode(value).decode("ascii"), "expected": _kv_expected(expected),
                "expires_at_ms": None if expires_at_ms is None else _kv_decimal(expires_at_ms)}
        result = self._kv_request("PUT", "/v1/collections/" + _collection(collection) + "/kv/entry", body)
        return self._kv_decode(_kv_read_put, result)

    def kv_delete(self, *, collection: str, namespace: str, key: str, expected: Expected = "any") -> bool:
        condition = _kv_expected(expected)
        query = urlencode({"namespace": _kv_text(namespace, "namespace", 255), "key": _kv_text(key, "key", 1024),
                           "expected": condition["revision"] if isinstance(condition, dict) else condition}, quote_via=quote)
        value = self._kv_request("DELETE", "/v1/collections/" + _collection(collection) + "/kv/entry?" + query)
        if not isinstance(value, dict) or type(value.get("deleted")) is not bool:
            raise TransportError("Invalid KV delete response; no retry performed")
        return value["deleted"]

    def kv_list(self, *, collection: str, namespace: str, prefix: str = "", after: str | None = None,
                limit: int = 100) -> KvPage:
        query = {"namespace": _kv_text(namespace, "namespace", 255), "prefix": _kv_text(prefix, "prefix", 1024, 0)}
        if after is not None:
            query["after"] = _kv_text(after, "after", 1024)
        query["limit"] = str(_int(limit, 1, 10000, "limit"))
        value = self._kv_request("GET", "/v1/collections/" + _collection(collection) + "/kv/entries?"
                                 + urlencode(query, quote_via=quote))
        def decode(page) -> KvPage:
            if not isinstance(page["items"], list):
                raise ValueError("items must be an array")
            return {"items": [_kv_read_entry(item) for item in page["items"]],
                    "next_after": None if page["next_after"] is None else _kv_text(page["next_after"], "next_after", 1024)}
        return self._kv_decode(decode, value)

    # Agent memory (Gap 1): see the Memory facade.

    def memory(self, binding: str) -> Memory:
        """Return the memory facade for one provisioned binding. No request is
        made, and the facade shares this client's connection settings."""
        return Memory(self, binding)

    def _memory_request(self, path: str, body: dict) -> dict:
        try:
            value = self._request("POST", path, body)
        except VLiteError as error:
            if error.status == 404 and error.code == "not_found":
                raise UnsupportedError("Server does not support memory routes; no fallback performed") from error
            raise
        if not isinstance(value, dict):
            raise TransportError("Invalid memory response; no retry performed")
        return value

    def changes(self, *, cursor: str | None = None, limit: int = 1000) -> dict:
        if type(limit) is not int or not 1 <= limit <= 10000:
            raise ValueError("limit must be an integer in 1..10000")
        query = {"limit": str(limit)}
        if cursor is not None:
            query["cursor"] = cursor
        return self._request("GET", "/v1/changes?" + urlencode(query))

    def list_webhooks(self) -> dict:
        return self._request("GET", "/v1/admin/webhooks")

    def create_webhook(self, request: dict) -> dict:
        return self._request("POST", "/v1/admin/webhooks", _object(request, "request"))

    def webhook_status(self, webhook_id: str) -> dict:
        if not re.fullmatch(r"[a-f0-9]{24}", webhook_id):
            raise ValueError("invalid webhook ID")
        return self._request("GET", "/v1/admin/webhooks/" + webhook_id)

    def delete_webhook(self, webhook_id: str) -> bool:
        if not re.fullmatch(r"[a-f0-9]{24}", webhook_id):
            raise ValueError("invalid webhook ID")
        return self._request("DELETE", "/v1/admin/webhooks/" + webhook_id)["deleted"]

    def auto_maintenance(self) -> dict:
        """``{"config": {..., "interval_ms", ...}, "state": {...}}``."""
        return self._request("GET", "/v1/admin/maintenance/auto")

    def set_auto_maintenance(self, policy: dict) -> dict:
        """Merge ``enabled``, ``interval_secs`` (10..3600), ``stale_ratio``,
        ``rebuild_on_low_recall``, ``min_recall``, ``quality_samples``, ``limits``.
        Memory-only (restart restores the environment policy)."""
        return self._request("PUT", "/v1/admin/maintenance/auto", _object(policy, "policy"))

    def tick_auto_maintenance(self) -> dict:
        """Run exactly one bounded auto-maintenance tick now."""
        return self._request("POST", "/v1/admin/maintenance/auto/tick")

    def backup_schedule(self) -> dict:
        """``{"config": {...}, "state": {...}}``."""
        return self._request("GET", "/v1/admin/backups/schedule")

    def run_backup_schedule(self) -> dict:
        """Run one scheduled backup cycle (backup, retention, drill) now:
        ``{"name", "manifest", "retention_deleted"}``."""
        return self._request("POST", "/v1/admin/backups/schedule/run")

    def set_backup_schedule(self, schedule: dict) -> dict:
        """Memory-only schedule change; retention deletes only with
        ``retention_enabled: True``."""
        return self._request("PUT", "/v1/admin/backups/schedule", _object(schedule, "schedule"))

    def list_backups(self) -> dict:
        return self._request("GET", "/v1/admin/backups")

    def create_backup(self) -> dict:
        return self._request("POST", "/v1/admin/backups")

    def verify_backup(self, name: str) -> dict:
        """Verify a backup against its manifest (422 verification_failed, also when
        the manifest or its metadata was altered)."""
        return self._request("POST", f"/v1/admin/backups/{_backup(name)}/verify")

    def drill_backup(self, name: str) -> dict:
        """Restore to a scratch file, validate and query it, then delete it; the
        active database is never touched (422 drill_failed)."""
        return self._request("POST", f"/v1/admin/backups/{_backup(name)}/drill")

    def slow_queries(self) -> dict:
        return self._request("GET", "/v1/admin/diagnostics/slow-queries")

    # Scoped credentials (bootstrap admin token only).

    def list_credentials(self) -> dict:
        """{"items": [Credential], "capacity": 256}; secrets are never returned."""
        return self._request("GET", "/v1/admin/credentials")

    def create_credential(self, request: NewCredential) -> IssuedCredential:
        """Returns {"credential", "token"}. The token is shown only once: hand it to
        its holder and never log or persist it in plaintext. 503
        ``credentials_commit_indeterminate`` (also on rotate/revoke) means the
        change is in effect but may not be durable and no token is returned:
        re-list and revoke or re-issue; it is not "no change"."""
        return self._request("POST", "/v1/admin/credentials", _object(request, "request"))

    def rotate_credential(self, id: str, *, grace_seconds: int | None = None) -> IssuedCredential:
        """Issue a replacement with identical scopes; the old credential expires at
        min(its expiry, now + grace_seconds) (server default 3600)."""
        body = {} if grace_seconds is None else {"grace_seconds": _int(grace_seconds, 0, 86400, "grace_seconds")}
        return self._request("POST", f"/v1/admin/credentials/{_credential(id)}/rotate", body)

    def revoke_credential(self, id: str) -> dict:
        """Revoke immediately (effective on the next request)."""
        return self._request("POST", f"/v1/admin/credentials/{_credential(id)}/revoke")

    def persistent_audit(self, *, limit: int = 100) -> dict:
        """``{"items": [...], "enabled": bool}``."""
        query = urlencode({"limit": str(_int(limit, 1, 1000, "limit"))})
        return self._request("GET", "/v1/admin/audit/persistent?" + query)
