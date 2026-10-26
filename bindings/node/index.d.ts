export type MetadataValue =
  | { type: "string"; value: string }
  | { type: "integer"; value: string }
  | { type: "float"; value: number }
  | { type: "boolean"; value: boolean }
  | { type: "geopoint"; value: { latitude: number; longitude: number } };
export type Metadata = Record<string, MetadataValue>;
export type NumericValue = Extract<MetadataValue, { type: "integer" | "float" }>;
export type Condition =
  | { op: "eq"; field: string; value: MetadataValue }
  | { op: "range"; field: string; min?: NumericValue | null; max?: NumericValue | null }
  | { op: "georadius"; field: string; latitude: number; longitude: number; radius_meters: number };
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
  create(path: string, dimensions: number, options?: CreateOptions): Promise<Database>;
  /**
   * `readOnly: true` shares the OS lock; mutations reject with `read_only`.
   * Writers require exclusive access (`locked` while readers exist, and vice versa).
   * Snapshot at open: close and reopen to see new commits. Parallel reads need separate processes.
   */
  open(path: string, options?: Credentials & DurabilityOptions & { readOnly?: boolean }): Promise<Database>;
  createInMemory(dimensions: number, options?: CreateOptions): Promise<Database>;
  /** Optional capabilities compiled into the loaded library (`graph` is true since 0.5.1); libraries predating the op report `{ graph: false }`. */
  capabilities(): Promise<{ graph: boolean }>;
  /** Whether the loaded library includes the property graph (every library since 0.5.1). */
  supportsGraph(): Promise<boolean>;
  /**
   * Create a NEW graph-enabled (V5) file without overwrite; never converts an existing file.
   * Rejects with `unsupported` on libraries older than 0.5.1 built without the graph.
   */
  createWithGraph(path: string, dimensions: number, options?: CreateOptions): Promise<Database>;
  openFromBytes(data: Uint8Array, options?: Credentials): Promise<Database>;
  /** Closed/copied log; new destination. Exactly one of sequence/timestampMs is required. */
  restoreTo(logPath: string, destination: string, options: Credentials & {
    sequence?: string | number; timestampMs?: string | number; baseBackup?: string;
  }): Promise<RestoreReport>;
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
  put(id: string, vector: number[], options?: Scope & { payload?: Uint8Array; metadata?: Metadata; expiresAtMs?: string | number }): Promise<void>;
  delete(id: string, options?: Scope & { expectedRevision?: ExpectedRevision }): Promise<boolean>;
  batch(operations: BatchOperation[], options?: Scope): Promise<void>;
  /** Snapshots and atomically inserts 1..4096 Float32Array-backed puts. */
  batchPutF32(operations: Float32PutOperation[], options?: Scope): Promise<void>;
  revision(id: string, options?: Scope): Promise<string | null>;
  search(vector: number[], k: number, options: Scope & { filter?: Condition[]; metric?: DistanceMetric; explain: true }): Promise<{ hits: Hit[]; stats: SearchStats }>;
  search(vector: number[], k: number, options?: Scope & { filter?: Condition[]; metric?: DistanceMetric; explain?: false }): Promise<Hit[]>;
  /** Opt-in typed search; the input is copied before it is queued to the native worker. */
  searchF32(vector: Float32Array, k: number, options: Scope & { filter?: Condition[]; metric?: DistanceMetric; explain: true }): Promise<{ hits: Hit[]; stats: SearchStats }>;
  searchF32(vector: Float32Array, k: number, options?: Scope & { filter?: Condition[]; metric?: DistanceMetric; explain?: false }): Promise<Hit[]>;
  /** Executes 1..128 equal-dimension queries in one worker request. */
  searchManyF32(vectors: Float32Array[], k: number, options: Scope & { filter?: Condition[]; metric?: DistanceMetric; explain: true }): Promise<Array<{ hits: Hit[]; stats: SearchStats }>>;
  searchManyF32(vectors: Float32Array[], k: number, options?: Scope & { filter?: Condition[]; metric?: DistanceMetric; explain?: false }): Promise<Hit[][]>;
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
  putWithExpiry(id: string, vector: number[], options?: Scope & {
    payload?: Uint8Array; metadata?: Metadata; expiresAtMs?: string | number | null;
  }): Promise<string>;
  updateMetadata(id: string, metadata: Metadata, options?: Scope): Promise<string>;
  searchGrouped(vector: number[], groups: number, hitsPerGroup: number, field: string, options?: FilterOptions): Promise<SearchGroup[]>;
  recommend(k: number, options?: FilterOptions & {
    positiveIds?: string[]; negativeIds?: string[]; positiveVectors?: number[][]; negativeVectors?: number[][];
  }): Promise<Hit[]>;
  facets(field: string, options?: Scope & { filter?: Condition[] }): Promise<FacetCount[]>;
  numericStats(field: string, options?: Scope & { filter?: Condition[] }): Promise<NumericStats>;
  searchGeo(vector: number[], k: number, field: string, latitude: number, longitude: number, radiusMeters: number,
    options?: FilterOptions & { sort?: "vector_distance" | "geo_distance" }): Promise<GeoHit[]>;
  configureVector(name: string, dimensions: number, options: Scope & {
    activeVersion: string; metric?: DistanceMetric; history?: number;
  }): Promise<void>;
  vectorSpecs(options?: Scope): Promise<Record<string, VectorSpec>>;
  recordVectors(id: string, options?: Scope): Promise<RecordVectors | null>;
  /** Replaces all additional vectors on a live record. */
  putVectors(id: string, options?: Scope & { dense?: Record<string, VectorVersion[]>; sparse?: Record<string, SparseVector> }): Promise<void>;
  putVectorVersion(id: string, name: string, version: string, vector: number[], options?: Scope): Promise<void>;
  pruneVectorHistory(name: string, history: number, options?: Scope): Promise<void>;
  searchNamed(name: string, vector: number[], k: number, options?: Scope & { version?: string }): Promise<Hit[]>;
  searchSparse(name: string, vector: SparseVector, k: number, options?: Scope): Promise<Hit[]>;
  searchDenseSparse(denseName: string, dense: number[], sparseName: string, sparse: SparseVector, k: number,
    options?: Scope & { version?: string; fusion?: FusionOptions }): Promise<HybridHit[]>;
  rerank(vector: number[], candidates: { id: string; distance?: number }[], limit: number, options?: Scope & {
    method?: "exact" | "mmr"; metric?: DistanceMetric; lambda?: number;
  }): Promise<Hit[]>;
  query(query: string, parameters?: Record<string, number[]>, options?: Scope): Promise<QueryOutput>;
  feedCursor(): Promise<string>;
  /** Omit cursor to start at the beginning; batches are never split. */
  changesAfter(cursor?: string | null, maxEvents?: number): Promise<ChangeBatch>;
  encryptionInfo(): Promise<{ is_encrypted: boolean; key_id: string | null }>;
  isEncrypted(): Promise<boolean>;
  encryptionKeyId(): Promise<string | null>;
  buildDiskAnn(path: string, options?: Scope & { metric?: "squared_l2" | "cosine" }): Promise<DiskAnnSource>;
  searchDiskAnn(path: string, vector: number[], k: number, options?: { efSearch?: number; cacheBytes?: number }): Promise<Hit[]>;
  toBytes(): Promise<Uint8Array>;
  /** Property graph (always compiled in since 0.5.1; libraries older than that reject every method with `unsupported`). */
  isGraphEnabled(): Promise<boolean>;
  listGraphs(): Promise<GraphInfo[]>;
  createGraph(name: string): Promise<GraphInfo>;
  /** Drop an empty graph. */
  dropGraph(name: string): Promise<void>;
  /** Applies graph and vector ops atomically; nothing is written on error. */
  graphWriteBatch(graph: string, ops: GraphOp[]): Promise<GraphBatchResult>;
  graphGetNode(graph: string, node: GraphSelector, options?: GraphReadOptions): Promise<GraphReadResult & { node: GraphNode | null }>;
  graphGetEdge(graph: string, edge: GraphSelector, options?: GraphReadOptions): Promise<GraphReadResult & { edge: GraphEdge | null }>;
  graphFindNodes(graph: string, options?: GraphReadOptions & {
    label?: string; conditions?: Condition[]; limit?: number; cursor?: string;
  }): Promise<GraphReadResult & { nodes: GraphNode[] }>;
  graphNeighbors(graph: string, node: GraphSelector, options?: GraphReadOptions & {
    direction?: GraphDirection; types?: string[]; edgeConditions?: Condition[]; limit?: number; cursor?: string;
  }): Promise<GraphReadResult & { edges: GraphEdge[]; nodes: GraphNode[] }>;
  graphTraverse(graph: string, start: GraphSelector[], options?: GraphReadOptions & {
    direction?: GraphDirection; types?: string[]; expandFilter?: GraphNodeFilter; returnFilter?: GraphNodeFilter;
    edgeFilter?: Condition[]; includePaths?: boolean;
  }): Promise<GraphReadResult & { nodes: { depth: number; node: GraphNode }[]; edges: GraphEdge[]; paths: GraphPath[] }>;
  graphShortestPath(graph: string, source: GraphSelector, destination: GraphSelector, options?: GraphReadOptions & {
    direction?: GraphDirection; types?: string[];
  }): Promise<GraphReadResult & { shortest_path: GraphPathOutcome }>;
  /** Plans one read request in native form (`{ op: "neighbors", graph, node: { key }, ... }`) without running it. */
  graphExplain(query: Record<string, unknown> & { op: string; graph: string }): Promise<GraphExplanation>;

}
export interface Credentials { key?: Uint8Array; passphrase?: string }
export interface Float32PutOperation { id: string; vector: Float32Array; payload?: Uint8Array; metadata?: Metadata }
/**
 * File handles default to `"periodic"` (background sync every `flushIntervalMs`, 1..60000, default 60000):
 * an OS crash or power loss can lose up to that much acknowledged work; a process crash loses nothing.
 * `"full"` syncs every commit. `"none"` is the only mode for in-memory databases (data is lost on close).
 */
export interface DurabilityOptions { durability?: "periodic" | "full" | "none"; flushIntervalMs?: number }
export interface CreateOptions extends Credentials, DurabilityOptions { format?: 3 | 4 | 5; commitTimestamps?: boolean }
export interface FilterOptions extends Scope { filter?: Condition[]; metric?: DistanceMetric }
export interface SearchGroup { value: MetadataValue; hits: Hit[] }
export interface FacetCount { value: MetadataValue; count: number }
export interface NumericStats { count: number; min: number | null; max: number | null; sum: number; average: number | null }
export interface GeoHit { id: string; vector_distance: number; geo_distance_meters: number }
export interface VectorSpec { dimensions: number; metric: DistanceMetric; active_version: string; history: number }
export interface VectorVersion { version: string; vector: number[] }
export type SparseVector = [number, number][];
export interface RecordVectors { dense: Record<string, VectorVersion[]>; sparse: Record<string, SparseVector> }
export interface FusionOptions { candidates?: number; rrfK?: number; vectorWeight?: number; textWeight?: number }
export interface QueryOutput { hits: Hit[]; groups: SearchGroup[]; facets: FacetCount[] }
export interface ChangeBase { sequence: string; ordinal: number; collection: string; committed_at_ms: string | null }
export type Change = ChangeBase & (
  | { kind: "put"; id: string; record: VectorRecord; expires_at_ms: string | null }
  | { kind: "delete"; id: string }
  | { kind: "metadata_update"; id: string; metadata: Metadata }
  | { kind: "collection_created"; dimensions: number; settings: CollectionSettings }
  | { kind: "settings_updated"; settings: CollectionSettings }
  | { kind: "collection_dropped" | "vectors_updated" }
  | { kind: "graph_created" | "graph_dropped"; graph: string }
  | { kind: "graph_batch"; graph: string; revision: string; operations: GraphChangeOperation[] }
);
/** One operation of a `graph_batch` event; IDs and records are decimal strings. */
export type GraphChangeOperation =
  | { op: "node_upserted"; id: string; key: string; labels: string[]; properties: Metadata; created: boolean }
  | { op: "node_patched"; id: string; key: string; add_labels: string[]; remove_labels: string[]; set: Metadata; unset: string[] }
  | { op: "node_deleted"; id: string; key: string }
  | { op: "edge_upserted"; id: string; key: string | null; source: string; source_key: string; relationship: string; destination: string; destination_key: string; properties: Metadata; created: boolean }
  | { op: "edge_patched"; id: string; key: string | null; set: Metadata; unset: string[] }
  | { op: "edge_deleted"; id: string; key: string | null }
  | { op: "attached" | "detached"; node: string; node_key: string; collection: string; record: string }
  | { op: "schema_updated"; schema: object };
export interface ChangeBatch { changes: Change[]; next: string; has_more: boolean; head: string }
export interface RestoreReport { restored_sequence: string; source_head: string; restored_at_ms: string | null }
export interface DiskAnnSource { lineage: string; sequence: string; collection_id: number; live_records: number }
/** Graph selectors: a key string, a non-negative integer ID, or an explicit `{ id }`/`{ key }` object. */
export type GraphSelector = string | number | bigint | { id: string | number } | { key: string };
export type GraphDirection = "out" | "in" | "both";
/** `"any"` (default), `"absent"`, or `{ revision: "decimal" }`. */
export type GraphExpected = "any" | "absent" | { revision: string | number };
export type GraphProjection = "all" | "none" | { only: string[] };
/** Omitted fields keep the engine defaults; values above the host ceiling reject with `graph_limit_exceeds_ceiling`. */
export interface GraphLimits {
  maxHops?: number; maxVisitedNodes?: number; maxEdgesExamined?: number; maxEdgesPerNode?: number; deadlineMs?: number;
  maxOutputNodes?: number; maxOutputEdges?: number; maxOutputPaths?: number;
}
export interface GraphReadOptions { limits?: GraphLimits; projection?: GraphProjection }
export interface GraphNodeFilter { labels?: string[]; conditions?: Condition[] }
export interface GraphInfo { name: string; node_count: number; edge_count: number }
/** Native snake_case batch ops; record IDs and revisions are decimal strings (or JSON integers). */
export type GraphOp =
  | { op: "create_node"; key: string; labels?: string[]; properties?: Metadata }
  | { op: "upsert_node"; key: string; labels?: string[]; properties?: Metadata; expected?: GraphExpected }
  | { op: "patch_node"; key: string; add_labels?: string[]; remove_labels?: string[]; set?: Metadata; unset?: string[]; expected?: GraphExpected }
  | { op: "delete_node"; key: string; expected?: GraphExpected }
  | { op: "create_edge"; key?: string; source: string; relationship: string; destination: string; properties?: Metadata }
  | { op: "patch_edge"; edge: { id: string | number } | { key: string }; set?: Metadata; unset?: string[]; expected?: GraphExpected }
  | { op: "delete_edge"; edge: { id: string | number } | { key: string }; expected?: GraphExpected }
  | { op: "attach"; node: string; collection: string; record: string | number }
  | { op: "detach"; node: string; collection: string }
  | { op: "put_vector"; collection: string; id: string | number; vector: number[]; payload_base64?: string; metadata?: Metadata }
  | { op: "delete_vector"; collection: string; id: string | number };
export interface GraphWrite { id: string; created: boolean; revision: string }
export type GraphOpOutcome =
  | { node: GraphWrite } | { edge: GraphWrite } | { node_deleted: { id: string } } | { edge_deleted: { id: string } }
  | { attached: { node: string } } | { detached: { node: string; record: string } } | "vector";
export interface GraphBatchResult { graph: string; revision: string | null; outcomes: GraphOpOutcome[] }
export interface GraphNode { id: string; key: string; labels: string[]; properties: Metadata; revision: string }
export interface GraphEdge {
  id: string; key: string | null; relationship: string; source: string; source_key: string;
  target: string; target_key: string; properties: Metadata; revision: string;
}
export interface GraphPath { nodes: string[]; edges: string[] }
export type GraphTruncation = "visited_limit" | "edge_limit" | "output_limit" | "deadline" | "cancelled"
  | { degree_limit: { node_ids: string[] } };
/** Every read runs on one snapshot identified by `view_sequence`; page with `cursor` (lowercase hex). */
export interface GraphReadResult {
  graph: string; view_sequence: string; complete: boolean; truncation: GraphTruncation[];
  stats: Record<string, unknown>; cursor: string | null;
}
export type GraphPathOutcome =
  | { outcome: "found"; path: GraphPath; nodes: GraphNode[]; edges: GraphEdge[] }
  | { outcome: "no_path_within_hops" }
  | { outcome: "exhausted"; truncation: GraphTruncation[] };
export interface GraphExplanation {
  query: Record<string, unknown>; limits: Record<string, number>; projection: GraphProjection; indexes: unknown[];
}
