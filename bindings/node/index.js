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
  async create(path, dimensions, { format } = {}) {
    // Format 3 remains the default; pass { format: 4 } for named collections and imports.
    const result = await this._request("0", { op: "create", path, dimensions, ...(format === undefined ? {} : { format }) });
    return new Database(this, result.handle);
  }
  async open(path) {
    const result = await this._request("0", { op: "open", path });
    return new Database(this, result.handle);
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
  async put(id, vector, { payload = new Uint8Array(), metadata = {}, collection, expectedRevision } = {}) {
    if (!(payload instanceof Uint8Array)) throw new TypeError("payload must be Uint8Array or Buffer");
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
