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
  async create(path, dimensions) {
    const result = await this._request("0", { op: "create", path, dimensions });
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
  get(id) { return this.#call("get", { id }); }
  async getMany(ids) { return (await this.#call("get_many", { ids })).records; }
  async put(id, vector, { payload = new Uint8Array(), metadata = {} } = {}) {
    if (!(payload instanceof Uint8Array)) throw new TypeError("payload must be Uint8Array or Buffer");
    await this.#call("put", { id, vector, payload_base64: Buffer.from(payload).toString("base64"), metadata });
  }
  async delete(id) { return (await this.#call("delete", { id })).deleted; }
  async batch(operations) { await this.#call("batch", { operations }); }
  async search(vector, k, { filter = [] } = {}) { return (await this.#call("search", { vector, k, filter })).hits; }
  async buildIndex() { await this.#call("build_index"); }
  async searchApproximateReady(vector, k, { efSearch = 100 } = {}) {
    return (await this.#call("search_approximate_ready", { vector, k, ef_search: efSearch })).hits;
  }
}
