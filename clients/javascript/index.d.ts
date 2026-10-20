export type MetadataValue =
  | { type: "string"; value: string }
  | { type: "integer"; value: string }
  | { type: "float"; value: number }
  | { type: "boolean"; value: boolean };
export type Metadata = Record<string, MetadataValue>;
export interface Put { vector: number[]; payload_base64?: string; metadata?: Metadata }
export interface VectorRecord extends Put { id: string; payload_base64: string; metadata: Metadata }
export type BatchOperation = ({ op: "put"; id: string } & Put) | { op: "delete"; id: string };
export type NumericValue = Extract<MetadataValue, { type: "integer" | "float" }>;
export type Condition =
  | { op: "eq"; field: string; value: MetadataValue }
  | { op: "range"; field: string; min?: NumericValue | null; max?: NumericValue | null };
export interface Search {
  vector: number[];
  k: number;
  mode?: "exact" | "approximate" | "approximate_ready";
  metric?: "squared_l2" | "cosine" | "inner_product";
  ef_search?: number;
  filter?: Condition[] | null;
}
export interface Hit { id: string; distance: number }
export interface Page { items: { id: string; metadata: Metadata }[]; next_after: string | null }
export interface DatabaseInfo { dimensions: number; format_version: number; records: number }
export interface IndexStatus {
  metric?: "squared_l2" | "cosine" | null;
  ready: boolean; buffered_ids: number; buffer_vector_bytes: number;
  max_buffer_ids: number; max_buffer_vector_bytes: number;
  config: { m: number; ef_construction: number; seed: number };
}
export class VLiteError extends Error {
  status: number; code: string; requestId: string | null;
  constructor(status: number, code: string, message: string, requestId: string | null);
}
export class TransportError extends Error {}
export class Client {
  constructor(baseUrl: string, token: string, options?: { timeoutMs?: number });
  health(): Promise<{ status: "ok" }>;
  ready(): Promise<{ status: "ready" }>;
  database(): Promise<DatabaseInfo>;
  metrics(): Promise<Record<string, number | boolean | null>>;
  get(id: string): Promise<VectorRecord>;
  put(id: string, record: Put): Promise<void>;
  delete(id: string): Promise<boolean>;
  listVectors(options?: { limit?: number; after?: string }): Promise<Page>;
  batch(operations: BatchOperation[]): Promise<void>;
  search(request: Search): Promise<Hit[]>;
  indexStatus(): Promise<IndexStatus>;
  buildIndex(options?: { metric?: "squared_l2" | "cosine" }): Promise<void>;
}
