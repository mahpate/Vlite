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
export interface VectorRecord { id: string; vector: number[]; payload_base64: string; metadata: Metadata }
export type BatchOperation =
  | { op: "put"; id: string; vector: number[]; payload_base64?: string; metadata?: Metadata }
  | { op: "delete"; id: string };
export interface Hit { id: string; distance: number }
export class EmbeddedError extends Error {
  code: string;
  constructor(code: string, message: string);
}
export class NativeLibrary {
  constructor(options?: { libraryPath?: string; addonPath?: string; maxPending?: number });
  create(path: string, dimensions: number): Promise<Database>;
  open(path: string): Promise<Database>;
  close(): Promise<void>;
}
export class Database {
  /** Instances are returned by NativeLibrary.create/open. */
  private constructor();
  close(): Promise<void>;
  info(): Promise<{ dimensions: number; records: number; format_version: number }>;
  get(id: string): Promise<VectorRecord>;
  getMany(ids: string[]): Promise<(VectorRecord | null)[]>;
  put(id: string, vector: number[], options?: { payload?: Uint8Array; metadata?: Metadata }): Promise<void>;
  delete(id: string): Promise<boolean>;
  batch(operations: BatchOperation[]): Promise<void>;
  search(vector: number[], k: number, options?: { filter?: Condition[] }): Promise<Hit[]>;
  buildIndex(): Promise<void>;
  searchApproximateReady(vector: number[], k: number, options?: { efSearch?: number }): Promise<Hit[]>;
}
