export type MetadataValue =
  | { type: "string"; value: string }
  | { type: "integer"; value: string }
  | { type: "float"; value: number }
  | { type: "boolean"; value: boolean };
export type Metadata = Record<string, MetadataValue>;
export interface Put { vector: number[]; payload_base64?: string; metadata?: Metadata; expires_at_ms?: number | null; ttl_ms?: number; ttl?: number }
/** `revision` (decimal u64 string) is returned by servers advertising `features.per_record_etags`. */
export interface VectorRecord extends Put { id: string; payload_base64: string; metadata: Metadata; revision?: string }
/**
 * Record plus its opaque strong ETag (including quotes). On servers advertising
 * `features.per_record_etags` the tag changes only when this record changes;
 * on format v4 files it survives a restart, on v1-v3 files a restart invalidates it.
 * Older servers invalidate every tag on any committed write or restart.
 */
export interface VersionedRecord { record: VectorRecord; etag: string }
/** Omit ifMatch for an unconditional write. If present it must be an exact strong tag from getVersioned; the client then checks /v1/capabilities before writing. */
export interface WriteOptions { ifMatch?: string }
export type BatchOperation = ({ op: "put"; id: string } & Put) | { op: "delete"; id: string };
export type NumericValue = Extract<MetadataValue, { type: "integer" | "float" }>;
export type Condition =
  | { op: "eq"; field: string; value: MetadataValue }
  | { op: "range"; field: string; min?: NumericValue | null; max?: NumericValue | null };
export type Metric = "squared_l2" | "cosine" | "inner_product";
export type Quantization = "none" | "scalar_u8";
/** `seed` is a decimal u64 string. */
export interface HnswConfig { m: number; ef_construction: number; seed: string }
interface SearchCommon {
  k: number;
  metric?: Metric;
  /** HNSW identity for approximate legs; omitted fields use the route's defaults (collection settings on collection routes). */
  config?: HnswConfig;
  /** Approximate modes only. With a filter it controls eligible-candidate retention, not a visit/time cap. */
  ef_search?: number;
  /**
   * AND of typed conditions; omitted, null or [] keeps every record. Approximate
   * filtered search returns at most k eligible records and may return fewer
   * (no exact fallback). Rejections are propagated, never retried.
   */
  filter?: Condition[] | null;
  quantization?: Quantization;
  /** 1..16; quantized candidates are reranked with exact distances. */
  rerank_multiplier?: number;
  /** Adds `plan` and `stats` to searchDetailed() responses. */
  explain?: boolean;
}
/**
 * exact/approximate/approximate_ready/auto. Approximate modes support
 * squared_l2/cosine only. Lazy "approximate" needs admin or the scoped
 * search_build operation; read-only/scoped search may use the others.
 */
export interface VectorSearch extends SearchCommon {
  vector: number[];
  mode?: "exact" | "approximate" | "approximate_ready" | "auto";
}
export interface Fusion { rrf_k?: number; vector_weight?: number; text_weight?: number; candidates?: number }
/** Reciprocal-rank fusion of a vector leg and a BM25 leg over the collection's text_fields. */
export interface HybridSearch extends SearchCommon {
  mode: "hybrid";
  vector?: number[];
  text?: string;
  fusion?: Fusion;
  vector_leg?: "exact" | "approximate_ready";
}
export type Search = VectorSearch | HybridSearch;
export interface Hit { id: string; distance: number }
/** Hybrid hits are ordered by score descending, then id. */
export interface HybridHit {
  id: string; score: number; distance: number | null; bm25: number | null;
  vector_rank: number | null; text_rank: number | null;
}
export interface QueryPlan {
  strategy: "exact_scan" | "exact_indexed" | "filtered_ann" | "ann";
  estimated_candidates: number | null; live_records: number; index_field: string | null; reason: string;
}
export interface SearchStats {
  records_scanned: number; distance_evaluations: number; quantized_distance_evaluations: number;
  graph_nodes_visited: number; buffered_records_merged: number; reranked_candidates: number; index_candidates: number;
}
/** `plan` is non-null only for mode "auto" with explain; `stats` is present only with explain. */
export interface SearchResponse { hits: (Hit | HybridHit)[]; plan?: QueryPlan | null; stats?: SearchStats }
export interface Page { items: { id: string; metadata: Metadata }[]; next_after: string | null }
export type Expected = "any" | "absent" | { revision: bigint };
export interface KvAddress { collection: string; namespace: string; key: string; signal?: AbortSignal }
export interface KvPutOptions extends KvAddress { value: Uint8Array; expected?: Expected; expiresAtMs?: bigint | null }
export interface KvListOptions {
  collection: string; namespace: string; prefix?: string; after?: string; limit?: number; signal?: AbortSignal;
}
export interface KvPutResult { version: bigint; expires_at_ms: bigint | null }
export interface KvEntry extends KvPutResult { key: string; value: Uint8Array }
export interface KvPage { items: KvEntry[]; next_after: string | null }
export interface DatabaseInfo { dimensions: number; format_version: number; records: number }
export interface IndexStatus {
  metric?: "squared_l2" | "cosine" | null;
  ready: boolean; buffered_ids: number; buffer_vector_bytes: number;
  max_buffer_ids: number; max_buffer_vector_bytes: number;
  config: { m: number; ef_construction: number; seed: number };
}
export interface RebuildRequest {
  metric?: "squared_l2" | "cosine"; config?: HnswConfig; quantization?: Quantization; rerank_multiplier?: number;
  limits?: { max_records?: number; max_snapshot_vector_bytes?: number };
}
export type RebuildStatus = Record<string, unknown>;

export interface MetadataIndex { field: string; kind: "eq" | "range" | "eq_range" }
export interface PlannerPolicy { exact_max_candidates: number; exact_max_selectivity_ppm: number }
export interface CollectionSettings {
  metric: Metric; hnsw: HnswConfig; quantization: Quantization; rerank_multiplier: number;
  planner: PlannerPolicy; metadata_indexes: MetadataIndex[]; text_fields: string[];
}
/** Omitted fields keep their current values. */
export type SettingsUpdate = Partial<Omit<CollectionSettings, "hnsw" | "planner">> & {
  hnsw?: Partial<HnswConfig>; planner?: Partial<PlannerPolicy>;
};
export interface CollectionInfo {
  /** Numeric collection ID (u16). */
  id: number; name: string; dimensions: number; live_records: number;
  settings: CollectionSettings; settings_persisted: boolean;
}
/** `declared_fields` is the number of declared text fields (names are in settings.text_fields). */
export interface TextIndexStatus { declared_fields: number; available: boolean; documents: number; terms: number; postings: number }
export interface CollectionDetail extends CollectionInfo { text_index: TextIndexStatus }
export interface CollectionList { items: CollectionInfo[]; format_version: number }
export interface AnnMemory {
  graph_nodes: number; graph_link_bytes: number; code_bytes: number; codebook_bytes: number;
  buffer_vector_bytes: number; resident_vector_bytes: number;
}
/** Superset of IndexStatus; `memory` is null without a graph; `settings_ready`: the graph matches the collection settings. */
export interface CollectionIndexStatus {
  ready: boolean; config: HnswConfig; metric: "squared_l2" | "cosine" | null; quantization: Quantization;
  rerank_multiplier: number; buffered_ids: number; buffer_vector_bytes: number; graph_nodes: number;
  stale_nodes: number; memory: AnnMemory | null; max_buffer_ids: number; max_buffer_vector_bytes: number;
  settings_ready: boolean;
}
export interface QualityOptions {
  config?: HnswConfig; metric?: Metric; quantization?: Quantization; rerank_multiplier?: number;
  samples?: number; k?: number; ef_search?: number; seed?: string; max_distance_evaluations?: number;
}
/** Measured on the ready index only (409 index_not_ready otherwise); never builds. */
export interface AnnQuality {
  recall_at_k: number | null; samples_used: number; distance_evaluations: number;
  skipped_reason: null | "budget" | "empty";
}

export type ImportPolicy = "reject_chunk" | "skip_invalid";
/** u64 counter: a number when it is a safe integer, otherwise an exact bigint (never rounded). */
export type Count = number | bigint;
export interface InvalidRow { row: number; code: string; message: string }
export interface ImportChunkReport {
  /** "rejected" is set by the client on ImportRejectedError.report. */
  outcome: "committed" | "already_committed" | "rejected";
  job: string; chunk: Count; rows_committed: Count; rows_skipped: Count;
  invalid_rows: number; invalid: InvalidRow[]; revision: string | null; status?: ImportStatus;
}
export interface ImportStatus {
  job: string; collection: string; collection_dropped: boolean;
  state: "open" | "finished" | "aborted"; last_chunk: Count;
  rows_committed: Count; rows_skipped: Count; last_digest: string | null;
  /** false until the first chunk is durable; such a job does not survive a restart. */
  durable: boolean;
}

export interface AutoMaintenancePolicy {
  enabled: boolean; interval_secs: number; stale_ratio: number; rebuild_on_low_recall: boolean; min_recall: number;
  quality_samples: number; limits: { max_records?: number; max_snapshot_vector_bytes?: number };
}
/** GET reports the interval as interval_ms; PUT accepts interval_secs (10..3600). */
export interface AutoMaintenance {
  config: Omit<AutoMaintenancePolicy, "interval_secs"> & { interval_ms: number };
  state: {
    ticks: number; skipped_busy: number; rebuilds_started: number; errors: number; last_tick_unix_ms: number | null;
    last_collection: string | null; last_action: unknown; last_error: string | null;
  };
}
export interface BackupSchedulePolicy {
  enabled: boolean; interval_secs: number; retention_enabled: boolean; retain_last: number; retain_days: number;
  drill_every: number; drill_max_bytes: number;
}
export interface BackupSchedule {
  config: BackupSchedulePolicy;
  state: {
    runs: number; failures: number; skipped_busy: number; last_success_unix_ms: number | null;
    last_failure_unix_ms: number | null; last_error: string | null; last_backup: string | null; drills: number;
    drill_failures: number; last_drill_unix_ms: number | null; retention_deleted: number; retained: number;
  };
}
export interface DrillResult { status: "passed" | "failed" | "skipped"; unix_ms: number; message: string }
export interface BackupManifest {
  schema: number; name: string; created_unix_ms: number; bytes: number; crc32: number; format_version: number;
  dimensions: number; live_records: number; commits: number; lineage: string | null; collections: unknown;
  verified_unix_ms: number | null; drill: DrillResult | null;
}
export interface BackupResult { name: string; manifest: BackupManifest | null }
export interface SlowQuery {
  request_id: string; unix_ms: number; route: string; collection: string | null;
  collection_id?: number; lineage?: string | null; mode: string;
  plan_strategy: string | null; k: number; ef_search: number | null; filter_fields: string[];
  duration_ms: number; stats: SearchStats; result_count: number;
}
export interface SlowQueries { items: SlowQuery[]; threshold_ms: number; capacity: number }

export type Operation =
  | "read" | "search" | "search_build" | "write" | "import" | "index" | "collections" | "diagnostics" | "backup";
export interface Credential {
  id: string; name: string; collections: string[]; operations: Operation[]; lineage: string | null;
  created_unix_ms: number; expires_unix_ms: number; revoked_unix_ms: number | null;
  rotated_from: string | null; state: "active" | "expired" | "revoked";
}
/** name is 1..64 characters; `collections` is ["*"] or names; operations nonempty; expiry mandatory (at most now + 365 days). */
export interface NewCredential { name: string; collections: string[]; operations: Operation[]; expires_unix_ms: number }
/** The secret token is returned only here, once. Never log or persist it. */
export interface IssuedCredential { credential: Credential; token: string }
export interface Capabilities {
  api_version: number; role: "admin" | "read_only" | "scoped"; format_version?: number;
  features: Record<string, boolean>;
  scope?: { credential_id: string; collections: string[]; operations: Operation[]; expires_unix_ms: number };
  [key: string]: unknown;
}

/** Canonical unsigned decimal string, 0..18446744073709551615, as returned by the server. */
export type MemoryU64 = string;
/** Canonical signed decimal string, -9223372036854775808..9223372036854775807, as returned by the server. */
export type MemoryI64 = string;
/**
 * Accepted inputs for a u64: a canonical decimal string, a safe-integer number or a bigint. An unsafe
 * number (beyond 2^53) is refused with a TypeError instead of being rounded; the client always sends a string.
 */
export type MemoryU64Input = MemoryU64 | number | bigint;
/** Accepted inputs for an i64; the same rules as MemoryU64Input. */
export type MemoryI64Input = MemoryI64 | number | bigint;
/** 1..256 UTF-8 bytes; the server validates the bytes. */
export type MemoryKey = string;
export type MemoryKind = "entity" | "episodic" | "fact" | "relation";
export type MemoryOperation = "remember" | "correct_facts" | "consolidate" | "forget" | "resume_erasure";
/** Session requires user. There is no tenant: it is derived from the credential. */
export interface MemoryScope { agent: MemoryKey; user?: MemoryKey; session?: MemoryKey }
export interface MemoryEffectiveScope { tenant: MemoryKey; agent: MemoryKey; user?: MemoryKey; session?: MemoryKey }
/** Exactly one variant: {absent: true} to create, or {revision} to guard an existing record. */
export type MemoryExpected<U extends MemoryU64Input = MemoryU64Input> =
  | { absent: true; revision?: never }
  | { revision: U; absent?: never };
/** Half-open validity window. Both members are required; null is an unbounded end, never a default. */
export interface MemoryInterval<I extends MemoryI64Input = MemoryI64> { valid_from_ms: I | null; valid_to_ms: I | null }
/** A retained revision, or revision 0 naming an ID created in the same bundle (request only). */
export interface MemoryRef<U extends MemoryU64Input = MemoryU64> { id: MemoryKey; revision: U }
export interface MemorySource { source_id: MemoryKey; source_kind: MemoryKey; uri?: string; transformation_id?: MemoryKey }
export interface MemoryProvenance extends MemorySource { content_digest: string }
/** Discriminated by `type`; an integer value is a decimal string (or an exact integer on input). */
export type MemoryScalar<I extends MemoryI64Input = MemoryI64> =
  | { type: "string"; value: string }
  | { type: "integer"; value: I }
  | { type: "float"; value: number }
  | { type: "boolean"; value: boolean };
export type MemoryProperties<I extends MemoryI64Input = MemoryI64> = Record<string, MemoryScalar<I>>;
export interface MemoryCondition { field: MemoryKey; op: "eq" | "lt" | "lte" | "gt" | "gte" | "exists"; value?: MemoryScalar<MemoryI64Input> }
/** AND of `all` and OR of `any`; the mandatory scope, kind and retention checks always apply as well. */
export interface MemoryFilter { all?: MemoryCondition[]; any?: MemoryCondition[] }
export interface MemoryEvidenceRef { collection: string; record: MemoryU64Input }
export interface MemoryEvidence {
  collection: string; record: MemoryU64Input; vector?: number[]; text?: string; payload_base64?: string;
  metadata?: MemoryProperties<MemoryI64Input>;
}
export interface MemoryAssertion {
  subject_id: MemoryKey; predicate: MemoryKey; slot_key: MemoryKey; destination_id: MemoryKey;
  interval: MemoryInterval<MemoryI64Input>; slot_expected: MemoryExpected; disagreement?: "reject" | "disputed";
}
export interface MemoryItem {
  id: MemoryKey; kind: MemoryKind; content: string; observed_at_ms: MemoryI64Input; importance?: number; confidence?: number;
  expires_at_ms?: MemoryU64Input; source: MemorySource; provenance?: MemorySource[]; references?: MemoryRef<MemoryU64Input>[];
  evidence?: MemoryEvidenceRef[]; metadata?: MemoryProperties<MemoryI64Input>; assertion?: MemoryAssertion;
  expected?: MemoryExpected; merge?: "none" | "merge_provenance";
}
/** Fields common to every mutation. The idempotency key is required: the client never generates or reuses one. */
export interface MemoryMutation {
  scope: MemoryScope; idempotency_key: MemoryKey; max_references?: number;
  /** The engine deadline in milliseconds. It is not the HTTP timeout, which is the client's timeoutMs. */
  deadline_ms?: number;
  require_complete_feed?: boolean;
}
export interface MemoryRememberRequest extends MemoryMutation { items: MemoryItem[]; evidence?: MemoryEvidence[] }
export interface MemoryRanking {
  selection?: "weighted" | "exact" | "mmr"; /** [wR, wD, wI] */ weights?: number[]; half_life_ms?: number;
  future_observations?: "reject" | "clamp"; lambda?: number;
}
export interface MemoryLimits {
  candidates?: number; vector_components?: number; evidence_bytes?: number; mmr_components?: number; deadline_ms?: number;
  max_hops?: number; max_visited_nodes?: number; max_edges_examined?: number; max_edges_per_node?: number;
  max_output_nodes?: number; max_output_edges?: number; max_output_paths?: number;
}
export interface MemoryProjection { snippets?: boolean; provenance?: boolean; paths?: boolean; metadata?: MemoryKey[] }
/** At least one of vector, text or entity_seeds. Text is lexical; the client never embeds. */
export interface MemoryRecallRequest {
  scope: MemoryScope; scopes?: MemoryScope[]; vector?: number[]; text?: string; embedding_provider?: MemoryKey;
  entity_seeds?: MemoryKey[]; kinds?: MemoryKind[]; filter?: MemoryFilter; as_of_ms?: MemoryI64Input; ranking?: MemoryRanking;
  limits?: MemoryLimits; projection?: MemoryProjection; mode?: "exact" | "approximate"; limit?: number; allow_partial?: boolean;
}
export interface MemorySlot { subject_id: MemoryKey; predicate: MemoryKey; slot_key: MemoryKey }
export interface MemoryFactsRequest {
  scope: MemoryScope; slot: MemorySlot; as_of_ms?: MemoryI64Input; history?: boolean; limit?: number; limits?: MemoryLimits;
  projection?: MemoryProjection; allow_partial?: boolean;
}
export interface MemoryCorrectionRequest extends MemoryMutation {
  slot: MemorySlot; series: MemoryRef<MemoryU64Input>; slot_revision: MemoryU64Input; interval: MemoryInterval<MemoryI64Input>;
  action: "close" | "replace" | "remove"; at_ms?: MemoryI64Input; correction_interval?: MemoryInterval<MemoryI64Input>;
  replacement?: MemoryItem; evidence?: MemoryEvidence[];
}
export interface MemoryInput { id: MemoryKey; revision: MemoryU64Input; disposition?: "retain" | "logical_delete" }
export interface MemoryConsolidateRequest extends MemoryMutation {
  inputs: MemoryInput[]; output: MemoryItem; evidence?: MemoryEvidence[]; dry_run?: boolean;
}
/** Exactly one variant, selected by its key. */
export type MemoryTarget<U extends MemoryU64Input = MemoryU64> =
  | { ids: MemoryRef<U>[]; series?: never; entity?: never; session?: never; scope?: never }
  | { series: MemoryRef<U>; ids?: never; entity?: never; session?: never; scope?: never }
  | { entity: MemoryRef<U>; ids?: never; series?: never; session?: never; scope?: never }
  | { session: true; ids?: never; series?: never; entity?: never; scope?: never }
  | { scope: true; ids?: never; series?: never; entity?: never; session?: never };
export interface MemoryForgetRequest extends MemoryMutation {
  target: MemoryTarget<MemoryU64Input>; mode: "logical_delete" | "expire"; expires_at_ms?: MemoryU64Input; allow_job?: boolean;
}
export interface MemoryReceiptRequest { scope: MemoryScope; operation: MemoryOperation; idempotency_key: MemoryKey }
export interface MemoryErasureRequest { scope: MemoryScope; job_id: MemoryKey }
export interface MemoryResumeRequest extends MemoryMutation {
  job_id: MemoryKey; revision: MemoryU64Input; generation: MemoryU64Input; resume_token: string;
}
export interface MemoryReceipt {
  id: MemoryKey; operation: MemoryOperation; scope: MemoryEffectiveScope; sequence: MemoryU64; retry_until_ms?: MemoryU64;
  digest_version: "memory-request-v1";
}
export interface MemoryOutcome {
  id: MemoryKey; action: "created" | "merged" | "noop" | "corrected" | "deleted" | "expired" | "retained_shared";
  revision: MemoryU64; expires_at_ms?: MemoryU64;
}
export interface MemoryFeed {
  mode: "ordinary_changes"; temporal_complete: false; ttl_complete: false; consumer_purge_acknowledged: false;
}
export interface MemoryErasureNotice {
  live_state_only: true; secure_erasure: false; external_copies: "operator_managed"; restore_requires_manifest: true;
  backup_history_purge: "not_verified"; manifest_id?: MemoryKey;
}
export interface MemoryErasure {
  id: MemoryKey; scope: MemoryEffectiveScope; target: MemoryTarget; revision: MemoryU64; generation: MemoryU64;
  state: "pending" | "logical"; logical_visibility: "hidden"; deleted_count: MemoryU64; remaining_count: MemoryU64 | null;
  resume_token?: string;
}
/** HTTP 200 or 202; a pending erasure is `state: "pending"` and is a normal result, never an error. */
export interface MemoryMutationResponse {
  receipt: MemoryReceipt; replayed: boolean; outcomes: MemoryOutcome[]; feed: MemoryFeed; remaining_work: MemoryU64 | null;
  state?: "logical" | "pending" | "retention_set"; erasure?: MemoryErasure; erasure_notice?: MemoryErasureNotice;
  /** Absent here; a dry-run plan carries `dry_run: true` (see MemoryConsolidateResponse). */
  dry_run?: never;
}
export interface MemoryReceiptResponse { result: MemoryMutationResponse }
export interface MemoryErasureResponse { erasure: MemoryErasure; erasure_notice: MemoryErasureNotice; feed: MemoryFeed }
export interface MemoryCompeting { id: MemoryKey; interval: MemoryInterval; revision: MemoryU64 }
export interface MemoryErrorDetail {
  field?: MemoryKey; limit?: MemoryU64; requested?: MemoryU64; competing?: MemoryCompeting[]; receipt_lookup_required?: boolean; code?: string;
}
export interface MemoryPlanResponse {
  dry_run: true; user_operations: MemoryU64; encoded_bytes: MemoryU64; conflicts: MemoryErrorDetail[]; remaining_work: MemoryU64 | null;
}
/** Discriminated by `dry_run`: true for a plan, absent for a committed mutation. */
export type MemoryConsolidateResponse = MemoryMutationResponse | MemoryPlanResponse;
export interface MemoryEvidenceView { collection: string; record: MemoryU64; snippet?: string }
export interface MemoryPath { nodes: MemoryU64[]; edges: MemoryU64[] }
export interface MemoryScores {
  R: number; D: number | null; I: number; S: number; dense_rank?: number; sparse_rank?: number; text_rank?: number;
  distance?: number; bm25?: number; sparse_score?: number; fused_rank?: number; fused_score?: number;
}
export interface MemoryView {
  id: MemoryKey; revision: MemoryU64; scope: MemoryEffectiveScope; kind: MemoryKind; content: string; observed_at_ms: MemoryI64;
  created_at_ms: MemoryU64; updated_at_ms: MemoryU64; importance: number; confidence?: number; expires_at_ms?: MemoryU64;
  status: "accepted" | "disputed"; stale: boolean; segment: MemoryInterval | null; evidence: MemoryEvidenceView[];
  scores: MemoryScores; provenance?: MemoryProvenance[]; references?: MemoryRef[]; paths?: MemoryPath[]; metadata?: MemoryProperties;
}
export interface MemoryReadMeta {
  scopes: MemoryEffectiveScope[]; as_of_ms: MemoryI64; ttl_clock_ms: MemoryU64; sequence: MemoryU64; complete: boolean;
  truncation: ("candidates" | "source_scan" | "vector_components" | "evidence_bytes" | "mmr_components" | "visited_nodes" | "edges"
    | "degree" | "deadline" | "output" | "deletion_barrier")[];
  limitations: ("historical_candidate_discovery_not_exhaustive" | "retained_state_only")[];
  mode: "exact_enumeration" | "approximate" | "bounded_expansion"; candidates_examined: MemoryU64; edges_charged: MemoryU64;
  vector_components: MemoryU64; mmr_components: MemoryU64; evidence_bytes: MemoryU64;
}
export interface MemoryRecallResponse {
  memories: MemoryView[]; meta: MemoryReadMeta; ranking_version: "memory-ranking-v1"; selection: "weighted" | "exact" | "mmr";
}
export interface MemoryFactView {
  series: MemoryRef; slot: MemorySlot; slot_revision: MemoryU64; destination_id: MemoryKey; interval: MemoryInterval;
  status: "accepted" | "disputed"; stale: boolean; evidence: MemoryEvidenceView[]; provenance?: MemoryProvenance[];
}
export interface MemoryFactsResponse { facts: MemoryFactView[]; meta: MemoryReadMeta }
/** The shared optional `error.details` object of a REST error; every member is optional. */
export interface MemoryErrorContext { field?: string; limit?: MemoryU64; requested?: MemoryU64; feature?: string; category?: string }
/** Only an abort signal; cancellation or timeout is a TransportError and never a retry. */
export interface MemoryCallOptions { signal?: AbortSignal }

export class VLiteError extends Error {
  status: number; code: string; requestId: string | null;
  /** The server's structured `error.details`, present only when it sent one. */
  details?: MemoryErrorContext;
  /** Set by importNdjson when it fails after a job was reserved or resumed; the job is not aborted. */
  importJob?: string;
  constructor(status: number, code: string, message: string, requestId: string | null, details?: MemoryErrorContext);
}
/** HTTP 422 import_rows_invalid: nothing was written and the job did not advance. */
export class ImportRejectedError extends VLiteError {
  report: ImportChunkReport;
}
export class TransportError extends Error {
  /** Set by importNdjson when it fails after a job was reserved or resumed; the job is not aborted. */
  importJob?: string;
}
/** The server lacks conditional writes, KV routes or memory routes (404 not_found); no fallback is performed. */
export class UnsupportedError extends Error {}

declare class Records {
  get(id: string): Promise<VectorRecord>;
  getVersioned(id: string): Promise<VersionedRecord>;
  put(id: string, record: Put, options?: WriteOptions): Promise<void>;
  delete(id: string, options?: WriteOptions): Promise<boolean>;
  updateMetadata(id: string, metadata: Metadata, options?: WriteOptions): Promise<void>;
  embedText(text: string, options?: { provider?: "hashing" | "http" }): Promise<{ vector: number[]; dimensions: number; provider: string }>;
  groupedSearch(request: object): Promise<object>;
  recommend(request: object): Promise<object>;
  facets(field: string, filter?: Condition[]): Promise<object>;
  numericStats(field: string, filter?: Condition[]): Promise<object>;
  geoSearch(request: object): Promise<object>;
  configureVector(name: string, options: { dimensions: number; metric?: Metric; activeVersion?: string; history?: number }): Promise<object>;
  putNamedVectors(id: string, vectors: object): Promise<object>;
  searchNamed(request: object): Promise<object>;
  searchSparse(request: object): Promise<object>;
  searchDenseSparse(request: object): Promise<object>;
  query(query: string, vectors?: Record<string, number[]>): Promise<object>;
  listVectors(options?: { limit?: number; after?: string }): Promise<Page>;
  batch(operations: BatchOperation[]): Promise<void>;
  search(request: Search): Promise<(Hit | HybridHit)[]>;
  searchDetailed(request: Search): Promise<SearchResponse>;
  rebuildStatus(): Promise<RebuildStatus>;
  startRebuild(request?: RebuildRequest): Promise<RebuildStatus>;
  pollRebuild(): Promise<RebuildStatus>;
  cancelRebuild(): Promise<RebuildStatus>;
  maintenanceStatus(): Promise<Record<string, unknown>>;
  maintenanceTick(): Promise<Record<string, unknown>>;
}

/** Routes under /v1/collections/{name}; omitted metric/config/quantization use the collection's settings. */
export class Collection extends Records {
  readonly name: string;
  info(): Promise<CollectionDetail>;
  indexStatus(): Promise<CollectionIndexStatus>;
  buildIndex(options?: { metric?: "squared_l2" | "cosine"; quantization?: Quantization; rerankMultiplier?: number }): Promise<void>;
  annQuality(options?: QualityOptions): Promise<AnnQuality>;
}

/**
 * Agent-memory operations of one binding (POST /v1/memory/{binding}/...); obtain with client.memory(binding).
 * Each method takes one request object and resolves with the full response. A failed call rejects with
 * VLiteError (code, status, requestId, details), UnsupportedError (404 not_found: the server has no memory
 * routes), TransportError (timeout, abort or lost connection: a mutation may or may not have committed;
 * reconcile with getReceipt) or TypeError (the request could not be represented; nothing was sent).
 * Nothing is retried, embedded, split or paged, and the client never invents an idempotency key.
 */
export class Memory {
  readonly binding: string;
  remember(request: MemoryRememberRequest, options?: MemoryCallOptions): Promise<MemoryMutationResponse>;
  recall(request: MemoryRecallRequest, options?: MemoryCallOptions): Promise<MemoryRecallResponse>;
  findFacts(request: MemoryFactsRequest, options?: MemoryCallOptions): Promise<MemoryFactsResponse>;
  correctFacts(request: MemoryCorrectionRequest, options?: MemoryCallOptions): Promise<MemoryMutationResponse>;
  consolidate(request: MemoryConsolidateRequest, options?: MemoryCallOptions): Promise<MemoryConsolidateResponse>;
  /** HTTP 202 resolves like 200: check `state` ("logical", "pending" or "retention_set"). */
  forget(request: MemoryForgetRequest, options?: MemoryCallOptions): Promise<MemoryMutationResponse>;
  getReceipt(request: MemoryReceiptRequest, options?: MemoryCallOptions): Promise<MemoryReceiptResponse>;
  getErasure(request: MemoryErasureRequest, options?: MemoryCallOptions): Promise<MemoryErasureResponse>;
  /** HTTP 202 resolves like 200: check `state`. */
  resumeErasure(request: MemoryResumeRequest, options?: MemoryCallOptions): Promise<MemoryMutationResponse>;
}

/** Legacy /v1 record routes act on the "default" collection with historical defaults. */
export class Client extends Records {
  constructor(baseUrl: string, token: string, options?: { timeoutMs?: number });
  health(): Promise<{ status: "ok" }>;
  ready(): Promise<{ status: "ready" }>;
  database(): Promise<DatabaseInfo>;
  kvGet(options: KvAddress): Promise<KvEntry>;
  kvPut(options: KvPutOptions): Promise<KvPutResult>;
  kvDelete(options: KvAddress & { expected?: Expected }): Promise<boolean>;
  kvList(options: KvListOptions): Promise<KvPage>;
  changes(options?: { cursor?: string; limit?: number }): Promise<object>;
  listWebhooks(): Promise<object>;
  createWebhook(request: object): Promise<object>;
  webhookStatus(id: string): Promise<object>;
  deleteWebhook(id: string): Promise<boolean>;
  metrics(): Promise<Record<string, unknown>>;
  /** GET /metrics?format=prometheus as text/plain. */
  prometheusMetrics(): Promise<string>;
  capabilities(): Promise<Capabilities>;
  indexStatus(): Promise<IndexStatus>;
  buildIndex(options?: { metric?: "squared_l2" | "cosine"; quantization?: Quantization; rerankMultiplier?: number }): Promise<void>;

  collection(name: string): Collection;
  /** Binding names match ^[a-z][a-z0-9_-]{0,62}$; a server without memory routes answers with UnsupportedError. */
  memory(binding: string): Memory;
  listCollections(): Promise<CollectionList>;
  createCollection(name: string, dimensions: number, settings?: SettingsUpdate): Promise<CollectionInfo>;
  getCollection(name: string): Promise<CollectionDetail>;
  dropCollection(name: string): Promise<boolean>;
  setCollectionSettings(name: string, settings: SettingsUpdate): Promise<{ collection: CollectionInfo; persisted: boolean }>;

  beginImport(collection: string): Promise<ImportStatus>;
  /** Replaying a committed chunk returns outcome "already_committed"; a rejected chunk throws ImportRejectedError. Never retried. */
  importChunk(job: string, chunk: string, operations: BatchOperation[], options?: { policy?: ImportPolicy }): Promise<ImportChunkReport>;
  importStatus(job: string): Promise<ImportStatus>;
  finishImport(job: string): Promise<ImportStatus>;
  abortImport(job: string): Promise<ImportStatus>;
  /**
   * Node only. The whole file is validated before a job is reserved or resumed (nothing is sent for a
   * missing, empty or malformed file). Pass `job` to resume: chunks before last_chunk are skipped and
   * last_chunk is replayed explicitly. A later failure rethrows the original error with `importJob`
   * set; the job is never aborted automatically.
   */
  importNdjson(path: string, collection: string, options?: { chunkSize?: number; policy?: ImportPolicy; job?: string }): Promise<ImportStatus>;

  autoMaintenance(): Promise<AutoMaintenance>;
  /** Memory-only; not persisted across restarts. */
  setAutoMaintenance(policy: Partial<AutoMaintenancePolicy>): Promise<AutoMaintenance>;
  /** Runs exactly one bounded auto-maintenance tick now. */
  tickAutoMaintenance(): Promise<Record<string, unknown>>;
  backupSchedule(): Promise<BackupSchedule>;
  setBackupSchedule(schedule: Partial<BackupSchedulePolicy>): Promise<BackupSchedule>;
  /** Runs one scheduled backup cycle now. */
  runBackupSchedule(): Promise<Record<string, unknown>>;
  listBackups(): Promise<{ items: (Record<string, unknown> & { name: string; manifest: BackupManifest | null })[] } & Record<string, unknown>>;
  createBackup(): Promise<BackupResult & { inspection: unknown; restart_required: boolean }>;
  verifyBackup(name: string): Promise<BackupResult & { verified: true }>;
  drillBackup(name: string): Promise<BackupResult & { drill: DrillResult; collections: unknown; probes: unknown }>;
  slowQueries(): Promise<SlowQueries>;

  listCredentials(): Promise<{ items: Credential[]; capacity: number }>;
  /**
   * The token is shown once; never log or persist it. 503 credentials_commit_indeterminate (also on
   * rotate/revoke) means the change IS in effect but may not be durable and no token is returned:
   * re-list and revoke or re-issue.
   */
  createCredential(request: NewCredential): Promise<IssuedCredential>;
  rotateCredential(id: string, options?: { graceSeconds?: number }): Promise<IssuedCredential & { previous: Credential }>;
  revokeCredential(id: string): Promise<{ credential: Credential }>;
  persistentAudit(options?: { limit?: number }): Promise<{ items: Record<string, unknown>[]; enabled: boolean }>;
}
