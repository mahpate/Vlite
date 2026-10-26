import { Worker } from "node:worker_threads";
import { fileURLToPath } from "node:url";
import { realpathSync } from "node:fs";
import { join } from "node:path";

const MAX_FLOAT32_VALUES = 2 * 1024 * 1024;
const MAX_FLOAT32_SEARCHES = 128;

export class EmbeddedError extends Error {
  constructor(code, message) { super(`${code}: ${message}`); this.name = "EmbeddedError"; this.code = code; }
}

function encode(value) {
  const text = JSON.stringify(value, (_, item) => {
    if (typeof item === "number" && !Number.isFinite(item)) throw new TypeError("Numbers must be finite");
    return item;
  });
  if (Buffer.byteLength(text) > 8 * 1024 * 1024) throw new RangeError("Request exceeds 8 MiB");
  return text;
}

export class NativeLibrary {
  #worker;
  #pending = new Map();
  #next = 0;
  #closed = false;
  #closing;
  #failure;
  #maxPending;
  constructor({ libraryPath, addonPath, maxPending = 64 } = {}) {
    if (!Number.isInteger(maxPending) || maxPending < 1 || maxPending > 1024) throw new RangeError("maxPending must be 1..1024");
    this.#maxPending = maxPending;
    const folder = fileURLToPath(new URL(`./prebuilds/${process.platform}-${process.arch}/`, import.meta.url));
    const name = process.platform === "win32" ? "vlite_ffi.dll" : process.platform === "darwin" ? "libvlite_ffi.dylib" : "libvlite_ffi.so";
    const library = realpathSync(libraryPath ?? join(folder, name));
    const addon = realpathSync(addonPath ?? join(folder, "vlite_node.node"));
    // App eval flags and loaders do not apply to this plain-JavaScript worker.
    this.#worker = new Worker(new URL("./worker.js", import.meta.url), { workerData: { library, addon }, execArgv: [] });
    this.#worker.on("message", ({ id, response }) => {
      const pending = this.#pending.get(id);
      if (!pending) return;
      this.#pending.delete(id);
      if (response.ok) pending.resolve(response.result);
      else pending.reject(new EmbeddedError(response.error.code, response.error.message));
    });
    const fail = (cause) => {
      this.#failure = new EmbeddedError("worker_failed", "Native worker stopped; mutation outcome may be unknown; no retry performed");
      if (cause instanceof Error) this.#failure.cause = cause;
      for (const pending of this.#pending.values()) pending.reject(this.#failure);
      this.#pending.clear();
      this.#closed = true;
    };
    this.#worker.on("error", fail);
    this.#worker.on("exit", () => { if (!this.#closed || this.#pending.size) fail(); });
  }
  _request(handle, request) {
    if (this.#failure) return Promise.reject(this.#failure);
    if (this.#closed) return Promise.reject(new EmbeddedError("closed", "Native library is closed"));
    if (this.#pending.size >= this.#maxPending) return Promise.reject(new EmbeddedError("busy", "Pending request limit reached; no request admitted"));
    let encoded;
    try { encoded = encode(request); }
    catch (error) { return Promise.reject(error); }
    return this.#send({ handle, request: encoded });
  }
  _typedRequest(handle, request, vectors, typed, dimensions) {
    if (this.#failure) return Promise.reject(this.#failure);
    if (this.#closed) return Promise.reject(new EmbeddedError("closed", "Native library is closed"));
    if (this.#pending.size >= this.#maxPending) return Promise.reject(new EmbeddedError("busy", "Pending request limit reached; no request admitted"));
    let encoded;
    try { encoded = encode(request); }
    catch (error) { return Promise.reject(error); }
    return this.#send({ handle, request: encoded, typed, vectors, dimensions }, [vectors.buffer]);
  }
  #send(message, transferList = []) {
    const id = ++this.#next;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      try { this.#worker.postMessage({ id, ...message }, transferList); }
      catch { this.#pending.delete(id); reject(new EmbeddedError("worker_failed", "Cannot admit operation; worker unavailable")); }
    });
  }
  async create(path, dimensions, { format, key, passphrase, commitTimestamps = false } = {}) {
    if (format === 5 || key !== undefined || passphrase !== undefined || commitTimestamps) {
      return this.#createFeature(path, dimensions, { format, key, passphrase, commitTimestamps });
    }
    const result = await this._request("0", { op: "create", path, dimensions, ...optional("format", format) });
    return new Database(this, result.handle);
  }
  _feature(op, fields = {}) { return this._request("0", { op: "features", request: { op, ...fields } }); }
  async #createFeature(path, dimensions, { format, key, passphrase, commitTimestamps = false }) {
    const result = await this._feature("create", { path, dimensions, options: {
      ...optional("format", format), credentials: credentials(key, passphrase), commit_timestamps: commitTimestamps,
    } });
    return new Database(this, result.handle);
  }
  createInMemory(dimensions, options = {}) { return this.#createFeature(null, dimensions, options); }
  /** Optional capabilities compiled into the loaded library, e.g. `{ graph: false }`. */
  async capabilities() {
    try { return await this._request("0", { op: "capabilities" }); }
    catch (error) {
      // Libraries older than the capabilities op reject it as an unknown variant.
      if (error instanceof EmbeddedError && error.code === "invalid_input") return { graph: false };
      throw error;
    }
  }
  /** Whether the loaded library was built with the source-only `graph` feature. */
  async supportsGraph() { return Boolean((await this.capabilities()).graph); }
  /** Create a NEW graph-enabled (V5) file without overwrite; never converts an existing file. */
  async createWithGraph(path, dimensions, { format, key, passphrase, commitTimestamps = false, flushIntervalMs } = {}) {
    const options = { credentials: credentials(key, passphrase), commit_timestamps: commitTimestamps,
      ...optional("format", format), ...optional("flush_interval_ms", flushIntervalMs) };
    const result = await graphCall(this._request("0", { op: "graph", request: { op: "create", path, dimensions, options } }));
    return new Database(this, result.handle);
  }
  /** Open a snapshot; readOnly shares file locks, rejects mutations, and excludes writers. */
  async open(path, { key, passphrase, readOnly = false } = {}) {
    const flag = readOnly ? { read_only: true } : {};
    const result = key === undefined && passphrase === undefined
      ? await this._request("0", { op: "open", path, ...flag })
      : await this._feature("open", { path, credentials: credentials(key, passphrase), ...flag });
    return new Database(this, result.handle);
  }
  async openFromBytes(data, { key, passphrase } = {}) {
    const result = await this._feature("open_from_bytes", { bytes_base64: bytesBase64(data), credentials: credentials(key, passphrase) });
    return new Database(this, result.handle);
  }
  restoreTo(logPath, destination, { sequence, timestampMs, baseBackup, key, passphrase } = {}) {
    return this._feature("restore_to", { log_path: logPath, destination, ...optional("base_backup", baseBackup),
      ...optional("sequence", decimalOption(sequence)), ...optional("timestamp_ms", decimalOption(timestampMs)), credentials: credentials(key, passphrase) });
  }
  close() {
    if (this.#closing) return this.#closing;
    if (this.#failure) return Promise.reject(this.#failure);
    if (this.#closed) return Promise.resolve();
    this.#closed = true;
    // Shutdown follows all already-admitted messages; do not terminate in-flight commits.
    this.#closing = this.#send({ shutdown: true }).finally(() => this.#worker.terminate());
    return this.#closing;
  }
}

export class Database {
  #library;
  #handle;
  #closing;
  constructor(library, handle) { this.#library = library; this.#handle = handle; }
  #call(op, fields = {}) {
    if (this.#handle === null) return Promise.reject(new EmbeddedError("closed", "Database is closed"));
    return this.#library._request(this.#handle, { ...fields, op });
  }
  #typedCall(request, vectors, typed, dimensions) {
    if (this.#handle === null) return Promise.reject(new EmbeddedError("closed", "Database is closed"));
    return this.#library._typedRequest(this.#handle, request, vectors, typed, dimensions);
  }
  close() {
    if (this.#closing) return this.#closing;
    if (this.#handle === null) return Promise.resolve();
    const handle = this.#handle;
    this.#handle = null;
    this.#closing = this.#library._request(handle, { op: "close" }).catch((error) => {
      // Backpressure rejects before admission, so retain ownership for an explicit retry.
      if (error instanceof EmbeddedError && error.code === "busy") {
        this.#handle = handle;
        this.#closing = undefined;
      }
      throw error;
    });
    return this.#closing;
  }
  info() { return this.#call("info"); }
  get(id, { collection } = {}) { return this.#call("get", { id, ...scope(collection) }); }
  async getMany(ids, { collection } = {}) { return (await this.#call("get_many", { ids, ...scope(collection) })).records; }
  async put(id, vector, { payload = new Uint8Array(), metadata = {}, collection, expectedRevision, expiresAtMs } = {}) {
    if (!(payload instanceof Uint8Array)) throw new TypeError("payload must be Uint8Array or Buffer");
    if (expiresAtMs !== undefined) {
      if (expectedRevision !== undefined) throw new TypeError("expiry writes do not support expectedRevision");
      await this.putWithExpiry(id, vector, { payload, metadata, collection, expiresAtMs });
      return;
    }
    const result = await this.#call("put", { id, vector, payload_base64: Buffer.from(payload).toString("base64"), metadata,
      ...scope(collection), ...optional("expected_revision", expectedRevision) });
    // Only conditional writes report the committed revision; unconditional puts keep resolving to undefined.
    return expectedRevision === undefined ? undefined : result.revision;
  }
  async delete(id, { collection, expectedRevision } = {}) {
    return (await this.#call("delete", { id, ...scope(collection), ...optional("expected_revision", expectedRevision) })).deleted;
  }
  async batch(operations, { collection } = {}) { await this.#call("batch", { operations, ...scope(collection) }); }
  async batchPutF32(operations, { collection } = {}) {
    if (!Array.isArray(operations) || operations.length < 1 || operations.length > 4096) {
      throw new RangeError("operations must contain 1..4096 puts");
    }
    const rows = operations.map((operation, index) => {
      if (!operation || typeof operation !== "object" || Array.isArray(operation)) {
        throw new TypeError(`operations[${index}] must be an object`);
      }
      const extra = Object.keys(operation).find((key) => !["id", "vector", "payload", "metadata"].includes(key));
      if (extra !== undefined) throw new TypeError(`operations[${index}] has unsupported field ${extra}`);
      if (typeof operation.id !== "string") throw new TypeError(`operations[${index}].id must be a string`);
      const vector = requireFloat32Array(operation.vector, `operations[${index}].vector`);
      if (vector.length === 0) throw new RangeError(`operations[${index}].vector must not be empty`);
      if (index && vector.length !== operations[0].vector.length) {
        throw new RangeError("all vectors must have the same dimensions");
      }
      if (vector.length * operations.length > MAX_FLOAT32_VALUES) {
        throw new RangeError("bulk vectors exceed 2097152 float32 values");
      }
      for (const value of vector) {
        if (!Number.isFinite(value)) throw new TypeError("vector values must be finite");
      }
      const payload = operation.payload ?? new Uint8Array();
      if (!(payload instanceof Uint8Array)) throw new TypeError(`operations[${index}].payload must be Uint8Array or Buffer`);
      return { id: operation.id, vector, payload_base64: Buffer.from(payload).toString("base64"),
        metadata: operation.metadata ?? {} };
    });
    const dimensions = rows[0].vector.length;
    const vectors = new Float32Array(dimensions * rows.length);
    rows.forEach((row, index) => vectors.set(row.vector, index * dimensions));
    const request = { dimensions, operations: rows.map(({ id, payload_base64, metadata }) => ({ id, payload_base64, metadata })),
      ...scope(collection) };
    await this.#typedCall(request, vectors, "batch_put_f32");
  }
  async revision(id, { collection } = {}) { return (await this.#call("revision", { id, ...scope(collection) })).revision; }
  async search(vector, k, { filter = [], metric, collection, explain = false } = {}) {
    const result = await this.#call("search", { vector, k, filter, ...optional("metric", metric), ...scope(collection),
      ...(explain ? { explain } : {}) });
    return explain ? result : result.hits;
  }
  async searchF32(vector, k, { filter = [], metric, collection, explain = false } = {}) {
    const snapshot = snapshotFloat32(vector);
    const request = { op: "search", k, filter, ...optional("metric", metric), ...scope(collection),
      ...(explain ? { explain: true } : {}) };
    const result = await this.#typedCall(request, snapshot, "search_f32");
    return explain ? result : result.hits;
  }
  async searchManyF32(vectors, k, { filter = [], metric, collection, explain = false } = {}) {
    if (!Array.isArray(vectors) || vectors.length < 1 || vectors.length > MAX_FLOAT32_SEARCHES) {
      throw new RangeError(`vectors must contain 1..${MAX_FLOAT32_SEARCHES} queries`);
    }
    const dimensions = requireFloat32Array(vectors[0], "vectors[0]").length;
    if (dimensions === 0 || dimensions * vectors.length > MAX_FLOAT32_VALUES) {
      throw new RangeError("search matrix must contain 1..2097152 float32 values");
    }
    const snapshot = new Float32Array(dimensions * vectors.length);
    vectors.forEach((vector, index) => {
      requireFloat32Array(vector, `vectors[${index}]`);
      if (vector.length !== dimensions) throw new RangeError("all query vectors must have the same dimensions");
      for (const value of vector) {
        if (!Number.isFinite(value)) throw new TypeError("vector values must be finite");
      }
      snapshot.set(vector, index * dimensions);
    });
    const request = { op: "search", k, filter, ...optional("metric", metric), ...scope(collection),
      ...(explain ? { explain: true } : {}) };
    const results = await this.#typedCall(request, snapshot, "search_many_f32", dimensions);
    return explain ? results : results.map((result) => result.hits);
  }
  searchAuto(vector, k, { filter = [], metric, collection, efSearch, quantization, rerankMultiplier, explain = false } = {}) {
    return this.#call("search", { mode: "auto", vector, k, filter, explain, ...optional("metric", metric), ...scope(collection),
      ...annFields({ efSearch, quantization, rerankMultiplier }) });
  }
  searchHybrid(text, k, { vector, filter = [], metric, collection, fusion, vectorLeg, efSearch, quantization,
    rerankMultiplier, explain = false } = {}) {
    return this.#call("search", { mode: "hybrid", text, k, filter, explain, ...optional("vector", vector),
      ...optional("metric", metric), ...scope(collection), ...optional("fusion", snake(fusion)),
      ...optional("vector_leg", vectorLeg), ...annFields({ efSearch, quantization, rerankMultiplier }) });
  }
  async buildIndex({ metric, collection, quantization, rerankMultiplier } = {}) {
    await this.#call("build_index", { ...optional("metric", metric), ...scope(collection),
      ...annFields({ quantization, rerankMultiplier }) });
  }
  async searchApproximateReady(vector, k, { efSearch = 100, metric, collection, quantization, rerankMultiplier } = {}) {
    return (await this.#call("search_approximate_ready", { vector, k, ef_search: efSearch, ...optional("metric", metric),
      ...scope(collection), ...annFields({ quantization, rerankMultiplier }) })).hits;
  }
  async listCollections() { return (await this.#call("list_collections")).collections; }
  createCollection(name, dimensions, settings) {
    return this.#call("create_collection", { name, dimensions, ...optional("settings", snake(settings)) });
  }
  async dropCollection(name) { await this.#call("drop_collection", { name }); }
  setCollectionSettings(name, settings) { return this.#call("set_collection_settings", { name, settings: snake(settings) }); }
  async nextImportJobId() { return (await this.#call("next_import_job_id")).job; }
  importChunk(job, chunk, operations, { collection, policy } = {}) {
    return this.#call("import_chunk", { job, chunk, operations, ...scope(collection), ...optional("policy", policy) });
  }
  importStatus(job) { return this.#call("import_status", { job }); }
  importFinish(job, { collection } = {}) { return this.#call("import_finish", { job, ...scope(collection) }); }
  importAbort(job, { collection } = {}) { return this.#call("import_abort", { job, ...scope(collection) }); }
  annQuality({ collection, metric, quantization, rerankMultiplier, samples, k, efSearch, seed, maxDistanceEvaluations } = {}) {
    return this.#call("ann_quality", { ...scope(collection), ...optional("metric", metric),
      ...annFields({ efSearch, quantization, rerankMultiplier }), ...optional("samples", samples), ...optional("k", k),
      ...optional("seed", seed), ...optional("max_distance_evaluations", maxDistanceEvaluations) });
  }
  annMemory({ collection, metric, quantization, rerankMultiplier } = {}) {
    return this.#call("ann_memory", { ...scope(collection), ...optional("metric", metric),
      ...annFields({ quantization, rerankMultiplier }) });
  }
  #feature(op, fields = {}) { return this.#call("features", { request: { op, ...fields } }); }
  async putWithExpiry(id, vector, { payload = new Uint8Array(), metadata = {}, expiresAtMs, collection } = {}) {
    const result = await this.#feature("put_with_expiry", { id, vector, payload_base64: bytesBase64(payload), metadata,
      expires_at_ms: decimalOption(expiresAtMs) ?? null, ...scope(collection) });
    return result.revision;
  }
  async updateMetadata(id, metadata, { collection } = {}) {
    return (await this.#feature("update_metadata", { id, metadata, ...scope(collection) })).revision;
  }
  searchGrouped(vector, groups, hitsPerGroup, field, { filter = [], metric, collection } = {}) {
    return this.#feature("search_grouped", { vector, groups, hits_per_group: hitsPerGroup, field, filter, ...optional("metric", metric), ...scope(collection) });
  }
  recommend(k, { positiveIds = [], negativeIds = [], positiveVectors = [], negativeVectors = [], filter = [], metric, collection } = {}) {
    return this.#feature("recommend", { k, positive_ids: positiveIds, negative_ids: negativeIds, positive_vectors: positiveVectors,
      negative_vectors: negativeVectors, filter, ...optional("metric", metric), ...scope(collection) });
  }
  facets(field, { filter = [], collection } = {}) { return this.#feature("facets", { field, filter, ...scope(collection) }); }
  numericStats(field, { filter = [], collection } = {}) { return this.#feature("numeric_stats", { field, filter, ...scope(collection) }); }
  searchGeo(vector, k, field, latitude, longitude, radiusMeters, { filter = [], metric, sort = "vector_distance", collection } = {}) {
    return this.#feature("search_geo", { vector, k, field, latitude, longitude, radius_meters: radiusMeters, filter, sort,
      ...optional("metric", metric), ...scope(collection) });
  }
  async configureVector(name, dimensions, { activeVersion, metric = "squared_l2", history = 0, collection } = {}) {
    await this.#feature("configure_vector", { name, dimensions, active_version: activeVersion, metric, history, ...scope(collection) });
  }
  vectorSpecs({ collection } = {}) { return this.#feature("vector_specs", scope(collection)); }
  recordVectors(id, { collection } = {}) { return this.#feature("record_vectors", { id, ...scope(collection) }); }
  async putVectors(id, { dense = {}, sparse = {}, collection } = {}) { await this.#feature("put_vectors", { id, dense, sparse, ...scope(collection) }); }
  async putVectorVersion(id, name, version, vector, { collection } = {}) { await this.#feature("put_vector_version", { id, name, version, vector, ...scope(collection) }); }
  async pruneVectorHistory(name, history, { collection } = {}) { await this.#feature("prune_vector_history", { name, history, ...scope(collection) }); }
  searchNamed(name, vector, k, { version, collection } = {}) { return this.#feature("search_named", { name, vector, k, ...optional("version", version), ...scope(collection) }); }
  searchSparse(name, vector, k, { collection } = {}) { return this.#feature("search_sparse", { name, vector, k, ...scope(collection) }); }
  searchDenseSparse(denseName, dense, sparseName, sparse, k, { version, fusion, collection } = {}) {
    return this.#feature("search_dense_sparse", { dense_name: denseName, dense, sparse_name: sparseName, sparse, k,
      ...optional("version", version), ...optional("fusion", snake(fusion)), ...scope(collection) });
  }
  rerank(vector, candidates, limit, { method = "exact", metric, lambda, collection } = {}) {
    return this.#feature("rerank", { vector, candidates, limit, method, ...optional("metric", metric), ...optional("lambda", lambda), ...scope(collection) });
  }
  query(query, parameters = {}, { collection } = {}) { return this.#feature("query", { query, parameters, ...scope(collection) }); }
  feedCursor() { return this.#feature("feed_cursor"); }
  changesAfter(cursor, maxEvents = 1000) { return this.#feature("changes_after", { cursor: cursor ?? null, max_events: maxEvents }); }
  encryptionInfo() { return this.#feature("encryption_info"); }
  async isEncrypted() { return (await this.encryptionInfo()).is_encrypted; }
  async encryptionKeyId() { return (await this.encryptionInfo()).key_id; }
  buildDiskAnn(path, { metric, collection } = {}) { return this.#feature("build_disk_ann", { path, ...optional("metric", metric), ...scope(collection) }); }
  searchDiskAnn(path, vector, k, { efSearch = 100, cacheBytes = 8 * 1024 * 1024 } = {}) {
    return this.#feature("search_disk_ann", { path, vector, k, ef_search: efSearch, cache_bytes: cacheBytes });
  }
  async toBytes() { return Buffer.from((await this.#feature("to_bytes")).bytes_base64, "base64"); }

  // Property graph (source-only `graph` feature). Nodes and edges are selected by key (string),
  // ID (bigint, safe integer or { id: "decimal" }) or { key }; IDs and revisions come back as
  // decimal strings. Libraries without the feature reject with EmbeddedError("unsupported").
  #graph(op, fields = {}) { return graphCall(this.#call("graph", { request: { op, ...fields } })); }
  async isGraphEnabled() { return (await this.#graph("is_graph_enabled")).graph_enabled; }
  async listGraphs() { return (await this.#graph("list_graphs")).graphs; }
  async createGraph(name) { return (await this.#graph("create_graph", { graph: name })).graph; }
  /** Drop an empty graph. */
  async dropGraph(name) { await this.#graph("drop_graph", { graph: name }); }
  /** Apply graph and vector ops atomically: `{ op: "create_node", key, ... }` etc. */
  async graphWriteBatch(graph, ops) { return this.#graph("batch", { graph, ops }); }
  async graphGetNode(graph, node, { limits, projection } = {}) {
    return this.#graph("get_node", { graph, node: graphRef(node), ...readFields({ limits, projection }) });
  }
  async graphGetEdge(graph, edge, { limits, projection } = {}) {
    return this.#graph("get_edge", { graph, edge: graphRef(edge), ...readFields({ limits, projection }) });
  }
  async graphFindNodes(graph, { label, conditions, limit, cursor, limits, projection } = {}) {
    return this.#graph("find_nodes", { graph, ...optional("label", label), ...optional("conditions", conditions),
      ...optional("limit", limit), ...optional("cursor", cursor), ...readFields({ limits, projection }) });
  }
  async graphNeighbors(graph, node, { direction, types, edgeConditions, limit, cursor, limits, projection } = {}) {
    return this.#graph("neighbors", { graph, node: graphRef(node), ...optional("direction", direction),
      ...optional("types", types), ...optional("edge_conditions", edgeConditions), ...optional("limit", limit),
      ...optional("cursor", cursor), ...readFields({ limits, projection }) });
  }
  async graphTraverse(graph, start, { direction, types, expandFilter, returnFilter, edgeFilter, includePaths = false,
    limits, projection } = {}) {
    if (!Array.isArray(start)) throw new TypeError("start must be an array of node selectors");
    return this.#graph("traverse", { graph, start: start.map(graphRef), include_paths: includePaths,
      ...optional("direction", direction), ...optional("types", types), ...optional("expand_filter", expandFilter),
      ...optional("return_filter", returnFilter), ...optional("edge_filter", edgeFilter), ...readFields({ limits, projection }) });
  }
  async graphShortestPath(graph, source, destination, { direction, types, limits, projection } = {}) {
    return this.#graph("shortest_path", { graph, source: graphRef(source), destination: graphRef(destination),
      ...optional("direction", direction), ...optional("types", types), ...readFields({ limits, projection }) });
  }
  /** Plan a read request (`{ op: "neighbors", graph, ... }`, native snake_case) without running it. */
  async graphExplain(query) { return (await this.#graph("explain", { query })).explain; }

}

// Omitted options are not sent, so the native side applies the collection's declared defaults.
function optional(name, value) { return value === undefined ? {} : { [name]: value }; }
function scope(collection) { return optional("collection", collection); }
function requireFloat32Array(value, name) {
  if (!(value instanceof Float32Array)) throw new TypeError(`${name} must be a Float32Array`);
  if (typeof SharedArrayBuffer !== "undefined" && value.buffer instanceof SharedArrayBuffer) {
    throw new TypeError(`${name} must not use SharedArrayBuffer`);
  }
  return value;
}
function snapshotFloat32(value) {
  requireFloat32Array(value, "vector");
  if (value.length < 1 || value.length > MAX_FLOAT32_VALUES) {
    throw new RangeError("vector must contain 1..2097152 float32 values");
  }
  const snapshot = new Float32Array(value);
  for (const item of snapshot) {
    if (!Number.isFinite(item)) throw new TypeError("vector values must be finite");
  }
  return snapshot;
}
function annFields({ efSearch, quantization, rerankMultiplier }) {
  return { ...optional("ef_search", efSearch), ...optional("quantization", quantization),
    ...optional("rerank_multiplier", rerankMultiplier) };
}
// Converts camelCase option keys to the native snake_case names. Unknown keys are kept so the
// native layer rejects them explicitly instead of the wrapper silently dropping them.
function snake(value) {
  if (Array.isArray(value)) return value.map(snake);
  if (value === null || typeof value !== "object") return value;
  return Object.fromEntries(Object.entries(value).map(([key, item]) =>
    [key.replace(/[A-Z]/g, (letter) => `_${letter.toLowerCase()}`), snake(item)]));
}

// Libraries that predate the graph op reject the envelope as an unknown variant.
function graphCall(promise) {
  return promise.catch((error) => {
    if (error instanceof EmbeddedError && error.code === "invalid_input" && error.message.includes("unknown variant `graph`")) {
      throw new EmbeddedError("unsupported", "Native library predates graph operations");
    }
    throw error;
  });
}
function graphRef(value) {
  if (typeof value === "string") return { key: value };
  if (typeof value === "bigint" && value >= 0n) return { id: value.toString() };
  if (Number.isSafeInteger(value) && value >= 0) return { id: String(value) };
  if (value !== null && typeof value === "object" && !Array.isArray(value)) return value;
  throw new TypeError("graph selector must be a key string, a non-negative integer ID, or a selector object");
}
function readFields({ limits, projection }) {
  return { ...optional("limits", snake(limits)), ...optional("projection", projection) };
}

function bytesBase64(data) {
  if (!(data instanceof Uint8Array)) throw new TypeError("expected Uint8Array or Buffer");
  return Buffer.from(data).toString("base64");
}
function credentials(key, passphrase) {
  if (key !== undefined && passphrase !== undefined) throw new TypeError("provide key or passphrase, not both");
  return { ...optional("key_base64", key === undefined ? undefined : bytesBase64(key)), ...optional("passphrase", passphrase) };
}
function decimalOption(value) {
  if (value === undefined || value === null) return undefined;
  if (typeof value === "number" && (!Number.isSafeInteger(value) || value < 0)) throw new TypeError("timestamp/sequence must be a safe non-negative integer or decimal string");
  return String(value);
}
