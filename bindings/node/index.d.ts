export type MetadataValue =
  | { type: "string"; value: string }
  | { type: "integer"; value: string }
  | { type: "float"; value: number }
  | { type: "boolean"; value: boolean };
export type Metadata = Record<string, MetadataValue>;
export type NumericValue = Extract<MetadataValue, { type: "integer" | "float" }>;
export type Condition =
  | { op: "eq"; field: string; value: MetadataValue }
  | { op: "range"; field: string; min?: NumericValue | null; max?: NumericValue | null };
/** `revision` is the record's current revision as a decimal u64 string. */
export interface VectorRecord { id: string; vector: number[]; payload_base64: string; metadata: Metadata; revision: string }
export type BatchOperation =
  | { op: "put"; id: string; vector: number[]; payload_base64?: string; metadata?: Metadata }
  | { op: "delete"; id: string };
export interface Hit { id: string; distance: number }
export type DistanceMetric = "squared_l2" | "cosine" | "inner_product";
export type Quantization = "none" | "scalar_u8";
/** `"absent"` requires that the ID does not exist; a decimal u64 string requires that exact revision. */
export type ExpectedRevision = "absent" | string;
/** Omit to address the `default` collection. Named collections require format 4. */
export interface Scope { collection?: string }
/** Omitted ANN fields use the collection's declared settings. */
export interface AnnOptions { quantization?: Quantization; rerankMultiplier?: number }
export interface SearchStats {
  records_scanned: number; distance_evaluations: number; quantized_distance_evaluations: number;
  graph_nodes_visited: number; buffered_records_merged: number; reranked_candidates: number; index_candidates: number;
}
export interface QueryPlan {
  strategy: "exact_scan" | "exact_indexed" | "filtered_ann" | "ann";
  estimated_candidates: number | null; live_records: number; index_field: string | null; reason: string;
}
export interface HybridHit {
  id: string; score: number; distance: number | null; bm25: number | null;
  vector_rank: number | null; text_rank: number | null;
}
export interface CollectionSettingsInput {
  metric?: DistanceMetric;
  hnsw?: { m?: number; efConstruction?: number; seed?: string };
  quantization?: Quantization;
  rerankMultiplier?: number;
  planner?: { exactMaxCandidates?: number; exactMaxSelectivityPpm?: number };
  metadataIndexes?: { field: string; kind: "eq" | "range" | "eq_range" }[];
  textFields?: string[];
}
export interface CollectionSettings {
  metric: DistanceMetric;
  hnsw: { m: number; ef_construction: number; seed: string };
  quantization: Quantization;
  rerank_multiplier: number;
  planner: { exact_max_candidates: number; exact_max_selectivity_ppm: number };
  metadata_indexes: { field: string; kind: "eq" | "range" | "eq_range" }[];
  text_fields: string[];
}
export interface CollectionInfo {
  id: number; name: string; dimensions: number; live_records: number;
  settings: CollectionSettings; settings_persisted: boolean;
}
export interface ImportChunkReport {
  outcome: "committed" | "already_committed" | "rejected";
  job: string; chunk: string; rows_committed: number; rows_skipped: number; invalid_rows: number;
  invalid: { row: number; code: string; message: string }[];
  revision: string | null;
}
export interface ImportStatus {
  job: string; collection: string; collection_dropped: boolean; state: "open" | "finished" | "aborted";
  last_chunk: string; rows_committed: number; rows_skipped: number; last_digest: string;
}
export interface AnnQualityReport {
  recall_at_k: number | null; samples_used: number; distance_evaluations: number; skipped_reason: string | null;
}
export interface AnnMemoryReport {
  graph_nodes: number; graph_link_bytes: number; code_bytes: number; codebook_bytes: number;
  buffer_vector_bytes: number; resident_vector_bytes: number;
}
export class EmbeddedError extends Error {
  code: string;
  constructor(code: string, message: string);
}
export class NativeLibrary {
  constructor(options?: { libraryPath?: string; addonPath?: string; maxPending?: number });
  /** `format` defaults to 3; 4 enables named collections and resumable imports. */
  create(path: string, dimensions: number, options?: { format?: 3 | 4 }): Promise<Database>;
  open(path: string): Promise<Database>;
  close(): Promise<void>;
}
export class Database {
  /** Instances are returned by NativeLibrary.create/open. */
  private constructor();
  close(): Promise<void>;
  info(): Promise<{ dimensions: number; records: number; format_version: number }>;
  get(id: string, options?: Scope): Promise<VectorRecord>;
  getMany(ids: string[], options?: Scope): Promise<(VectorRecord | null)[]>;
  put(id: string, vector: number[], options: Scope & { payload?: Uint8Array; metadata?: Metadata; expectedRevision: ExpectedRevision }): Promise<string>;
  put(id: string, vector: number[], options?: Scope & { payload?: Uint8Array; metadata?: Metadata }): Promise<void>;
  delete(id: string, options?: Scope & { expectedRevision?: ExpectedRevision }): Promise<boolean>;
  batch(operations: BatchOperation[], options?: Scope): Promise<void>;
  revision(id: string, options?: Scope): Promise<string | null>;
  search(vector: number[], k: number, options: Scope & { filter?: Condition[]; metric?: DistanceMetric; explain: true }): Promise<{ hits: Hit[]; stats: SearchStats }>;
  search(vector: number[], k: number, options?: Scope & { filter?: Condition[]; metric?: DistanceMetric; explain?: false }): Promise<Hit[]>;
  /** Planned search; `plan` and `stats` are present only with `explain: true`. */
  searchAuto(vector: number[], k: number, options?: Scope & AnnOptions & { filter?: Condition[]; metric?: DistanceMetric; efSearch?: number; explain?: boolean }): Promise<{ hits: Hit[]; plan?: QueryPlan; stats?: SearchStats }>;
  /** BM25 over the collection's declared text fields fused with an optional vector leg. */
  searchHybrid(text: string, k: number, options?: Scope & AnnOptions & {
    vector?: number[]; filter?: Condition[]; metric?: DistanceMetric; efSearch?: number; explain?: boolean;
    vectorLeg?: "exact" | "approximate_ready";
    fusion?: { candidates?: number; rrfK?: number; vectorWeight?: number; textWeight?: number };
  }): Promise<{ hits: HybridHit[]; stats?: SearchStats }>;
  buildIndex(options?: Scope & AnnOptions & { metric?: "squared_l2" | "cosine" }): Promise<void>;
  searchApproximateReady(vector: number[], k: number, options?: Scope & AnnOptions & { efSearch?: number; metric?: "squared_l2" | "cosine" }): Promise<Hit[]>;
  listCollections(): Promise<CollectionInfo[]>;
  createCollection(name: string, dimensions: number, settings?: CollectionSettingsInput): Promise<CollectionInfo>;
  dropCollection(name: string): Promise<void>;
  /** Full replacement: omitted settings revert to engine defaults. */
  setCollectionSettings(name: string, settings: CollectionSettingsInput): Promise<{ persisted: boolean }>;
  nextImportJobId(): Promise<string>;
  importChunk(job: string, chunk: string, operations: BatchOperation[], options?: Scope & { policy?: "reject_chunk" | "skip_invalid" }): Promise<ImportChunkReport>;
  importStatus(job: string): Promise<ImportStatus>;
  importFinish(job: string, options?: Scope): Promise<ImportStatus>;
  importAbort(job: string, options?: Scope): Promise<ImportStatus>;
  annQuality(options?: Scope & AnnOptions & {
    metric?: "squared_l2" | "cosine"; samples?: number; k?: number; efSearch?: number; seed?: string; maxDistanceEvaluations?: number;
  }): Promise<AnnQualityReport>;
  annMemory(options?: Scope & AnnOptions & { metric?: "squared_l2" | "cosine" }): Promise<AnnMemoryReport>;
}