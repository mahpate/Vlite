export type MetadataValue =
  | { type: "string"; value: string }
  | { type: "integer"; value: string }
  | { type: "float"; value: number }
  | { type: "boolean"; value: boolean };
export type Metadata = Record<string, MetadataValue>;
export interface Put { vector: number[]; payload_base64?: string; metadata?: Metadata }
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

export class VLiteError extends Error {
  status: number; code: string; requestId: string | null;
  /** Set by importNdjson when it fails after a job was reserved or resumed; the job is not aborted. */
  importJob?: string;
  constructor(status: number, code: string, message: string, requestId: string | null);
}
/** HTTP 422 import_rows_invalid: nothing was written and the job did not advance. */
export class ImportRejectedError extends VLiteError {
  report: ImportChunkReport;
}
export class TransportError extends Error {
  /** Set by importNdjson when it fails after a job was reserved or resumed; the job is not aborted. */
  importJob?: string;
}
/** Conditional write refused because GET /v1/capabilities did not report api_version 1 with features.conditional_writes === true. Nothing was sent. */
export class UnsupportedError extends Error {}

declare class Records {
  get(id: string): Promise<VectorRecord>;
  getVersioned(id: string): Promise<VersionedRecord>;
  put(id: string, record: Put, options?: WriteOptions): Promise<void>;
  delete(id: string, options?: WriteOptions): Promise<boolean>;
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

/** Legacy /v1 record routes act on the "default" collection with historical defaults. */
export class Client extends Records {
  constructor(baseUrl: string, token: string, options?: { timeoutMs?: number });
  health(): Promise<{ status: "ok" }>;
  ready(): Promise<{ status: "ready" }>;
  database(): Promise<DatabaseInfo>;
  metrics(): Promise<Record<string, unknown>>;
  /** GET /metrics?format=prometheus as text/plain. */
  prometheusMetrics(): Promise<string>;
  capabilities(): Promise<Capabilities>;
  indexStatus(): Promise<IndexStatus>;
  buildIndex(options?: { metric?: "squared_l2" | "cosine"; quantization?: Quantization; rerankMultiplier?: number }): Promise<void>;

  collection(name: string): Collection;
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
