import { Worker } from "node:worker_threads";
import { fileURLToPath } from "node:url";
import { realpathSync } from "node:fs";
import { join } from "node:path";

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
  #send(message) {
    const id = ++this.#next;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      try { this.#worker.postMessage({ id, ...message }); }
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
  async open(path, { key, passphrase } = {}) {
    const result = key === undefined && passphrase === undefined
      ? await this._request("0", { op: "open", path })
      : await this._feature("open", { path, credentials: credentials(key, passphrase) });
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
  async revision(id, { collection } = {}) { return (await this.#call("revision", { id, ...scope(collection) })).revision; }
  async search(vector, k, { filter = [], metric, collection, explain = false } = {}) {
    const result = await this.#call("search", { vector, k, filter, ...optional("metric", metric), ...scope(collection),
      ...(explain ? { explain } : {}) });
    return explain ? result : result.hits;
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

}

// Omitted options are not sent, so the native side applies the collection's declared defaults.
function optional(name, value) { return value === undefined ? {} : { [name]: value }; }
function scope(collection) { return optional("collection", collection); }
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
