function id(value) {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]{0,19})$/.test(value) || BigInt(value) > 18446744073709551615n) {
    throw new TypeError("ID must be a canonical decimal u64 string");
  }
  return value;
}

function collectionName(value) {
  if (typeof value !== "string" || !/^[a-z][a-z0-9_-]{0,62}$/.test(value)) {
    throw new TypeError("collection name must match ^[a-z][a-z0-9_-]{0,62}$");
  }
  return value;
}

function credentialId(value) {
  if (typeof value !== "string" || !/^[a-z2-7]{12}$/.test(value)) {
    throw new TypeError("credential ID must be 12 characters of [a-z2-7]");
  }
  return value;
}

// Generated snapshot names only; never a path, traversal or URL syntax.
function backupName(value) {
  if (typeof value !== "string" || !/^[A-Za-z0-9][A-Za-z0-9.-]{0,95}$/.test(value) || value.includes("..")) {
    throw new TypeError("backup name must be a generated snapshot name");
  }
  return value;
}

function integer(value, low, high, name) {
  if (!Number.isInteger(value) || value < low || value > high) throw new TypeError(`${name} must be an integer in ${low}..${high}`);
  return value;
}

function object(value, name) {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new TypeError(`${name} must be an object`);
  return value;
}

function policy(value) {
  if (value !== "reject_chunk" && value !== "skip_invalid") throw new TypeError("policy must be reject_chunk or skip_invalid");
  return value;
}

// One strong RFC 9110 entity tag of visible ASCII: no W/ prefix, wildcard,
// whitespace, control characters, inner quotes or comma-separated lists.
const ETAG = /^"[\x21\x23-\x7e]*"$/;

function writeOptions(options) {
  if (options === undefined) return undefined;
  if (typeof options !== "object" || options === null || Array.isArray(options)
    || Object.keys(options).some((key) => key !== "ifMatch")) {
    throw new TypeError("options must be an object with only ifMatch");
  }
  // A present-but-unusable ifMatch is rejected rather than downgraded to an unconditional write.
  if (!Object.hasOwn(options, "ifMatch")) return undefined;
  const tag = options.ifMatch;
  if (typeof tag !== "string" || !ETAG.test(tag)) {
    throw new TypeError("ifMatch must be one exact strong entity tag returned by getVersioned");
  }
  return tag;
}

function indexBody(metric, quantization, rerankMultiplier) {
  return {
    ...(metric === undefined ? {} : { metric }),
    ...(quantization === undefined ? {} : { quantization }),
    ...(rerankMultiplier === undefined ? {} : { rerank_multiplier: integer(rerankMultiplier, 1, 16, "rerankMultiplier") }),
  };
}

export class VLiteError extends Error {
  constructor(status, code, message, requestId) {
    super(`HTTP ${status}: ${code}: ${message}`);
    this.name = "VLiteError";
    this.status = status;
    this.code = code;
    this.requestId = requestId;
  }
}

/** HTTP 422 import_rows_invalid: nothing was written and the job did not advance. */
export class ImportRejectedError extends VLiteError {
  constructor(status, code, message, requestId, report) {
    super(status, code, message, requestId);
    this.name = "ImportRejectedError";
    this.report = report;
  }
}

export class TransportError extends Error {
  constructor(message = "Request failed or response was invalid; mutation outcome may be unknown; no retry performed") {
    super(message);
    this.name = "TransportError";
  }
}

export class UnsupportedError extends Error {
  constructor() {
    super("Server does not advertise conditional_writes (API version 1); no write was sent");
    this.name = "UnsupportedError";
  }
}

// Integer literals outside Number's safe range (u64 counters) become exact
// BigInts instead of silently rounding; fails if the runtime hides the source.
function exactJson(raw) {
  return JSON.parse(raw, (_, value, context) => {
    if (typeof value !== "number" || Number.isSafeInteger(value) || !Number.isInteger(value)) return value;
    const source = context?.source;
    if (typeof source !== "string") throw new TypeError("cannot parse integer exactly");
    return /^-?\d+$/.test(source) ? BigInt(source) : value;
  });
}

function exactCount(value) {
  if ((typeof value === "bigint" && value >= 0n) || (Number.isSafeInteger(value) && value >= 0)
    || (typeof value === "string" && /^\d+$/.test(value))) return BigInt(value);
  throw new TransportError("import status has an invalid last_chunk");
}

// The transport closes over the token so it never appears on any object.
function transport(base, token, timeout) {
  async function request(method, path, body, { ifMatch, etag = false, text = false } = {}) {
    // Reject nonfinite numbers rather than JSON.stringify silently converting them to null.
    const encoded = body === undefined ? undefined : JSON.stringify(body, (_, value) => {
      if (typeof value === "number" && !Number.isFinite(value)) throw new TypeError("Numbers must be finite");
      return value;
    });
    let response, value, raw;
    try {
      response = await fetch(base + path, {
        method, body: encoded, redirect: "error", credentials: "omit", cache: "no-store",
        signal: AbortSignal.timeout(timeout),
        headers: { Authorization: `Bearer ${token}`, Accept: text ? "text/plain" : "application/json",
          ...(encoded === undefined ? {} : { "Content-Type": "application/json" }),
          ...(ifMatch === undefined ? {} : { "If-Match": ifMatch }) },
      });
      raw = await response.text();
      if (!(text && response.ok)) {
        try { value = raw ? exactJson(raw) : null; }
        catch { if (response.ok) throw new TransportError(); }
      }
    } catch { throw new TransportError(); }
    if (!response.ok) {
      const detail = value?.error;
      const requestId = response.headers.get("x-request-id");
      if (response.status === 422 && detail?.code === "import_rows_invalid") {
        const { error: _, ...report } = value;
        throw new ImportRejectedError(422, detail.code, detail.message ?? "", requestId, { ...report, outcome: "rejected" });
      }
      throw new VLiteError(response.status, detail?.code ?? "http_error",
        detail?.message ?? "Server returned a nonstandard error response", requestId);
    }
    if (text) {
      if (!(response.headers.get("content-type") ?? "").startsWith("text/plain")) throw new TransportError();
      return raw;
    }
    if (!etag) return value;
    // Multiple ETag fields are combined with ", " by fetch and therefore fail validation.
    const tag = response.headers.get("etag");
    if (tag === null || !ETAG.test(tag)) throw new TransportError();
    return { record: value, etag: tag };
  }
  // Older servers ignore If-Match; a fresh check per write avoids silently
  // downgrading to an unconditional mutation (never cached across restarts).
  async function conditional(options) {
    const tag = writeOptions(options);
    if (tag === undefined) return undefined;
    let caps;
    try { caps = await request("GET", "/v1/capabilities"); }
    catch (error) { if (!(error instanceof VLiteError && error.status === 404)) throw error; }
    if (typeof caps !== "object" || caps === null || Array.isArray(caps) || caps.api_version !== 1
      || caps.features?.conditional_writes !== true) throw new UnsupportedError();
    return tag;
  }
  return { request, conditional };
}

async function* ndjsonChunks(path, size) {
  const [{ createReadStream }, { createInterface }] = await Promise.all([import("node:fs"), import("node:readline")]);
  const lines = createInterface({ input: createReadStream(path, { encoding: "utf8" }), crlfDelay: Infinity });
  let rows = [];
  let number = 0;
  try {
    for await (const line of lines) {
      number += 1;
      if (!line.trim()) continue;
      let row;
      try { row = JSON.parse(line); } catch { throw new TypeError(`line ${number} is not valid JSON`); }
      if (typeof row !== "object" || row === null || Array.isArray(row)) throw new TypeError(`line ${number} must be a JSON object`);
      rows.push(row);
      if (rows.length === size) { yield rows; rows = []; }
    }
    if (rows.length) yield rows;
  } finally { lines.close(); }
}

/** Record, batch, search and index calls under one path prefix. */
class Records {
  #t;
  #prefix;
  #legacy;
  constructor(t, prefix, legacy) {
    this.#t = t;
    this.#prefix = prefix;
    this.#legacy = legacy;
  }
  get(recordId) { return this.#t.request("GET", `${this.#prefix}/vectors/${id(recordId)}`); }
  getVersioned(recordId) {
    return this.#t.request("GET", `${this.#prefix}/vectors/${id(recordId)}`, undefined, { etag: true });
  }
  async put(recordId, record, options) {
    const path = `${this.#prefix}/vectors/${id(recordId)}`;
    await this.#t.request("PUT", path, record, { ifMatch: await this.#t.conditional(options) });
  }
  async delete(recordId, options) {
    const path = `${this.#prefix}/vectors/${id(recordId)}`;
    return (await this.#t.request("DELETE", path, undefined, { ifMatch: await this.#t.conditional(options) })).deleted;
  }
  listVectors({ limit = 25, after } = {}) {
    if (!Number.isInteger(limit) || limit < 1 || limit > 100) throw new TypeError("limit must be 1..100");
    return this.#t.request("GET", `${this.#prefix}/vectors?limit=${limit}${after === undefined ? "" : `&after=${id(after)}`}`);
  }
  async batch(operations) {
    for (const operation of operations) id(operation.id);
    await this.#t.request("POST", `${this.#prefix}/batches`, { operations });
  }
  async search(request) { return (await this.searchDetailed(request)).hits; }
  searchDetailed(request) { return this.#t.request("POST", `${this.#prefix}/search`, request); }
  embedText(text, { provider = "hashing" } = {}) {
    if (typeof text !== "string" || Buffer.byteLength(text, "utf8") > 16384) throw new TypeError("text must be at most 16384 UTF-8 bytes");
    if (provider !== "hashing" && provider !== "http") throw new TypeError("provider must be hashing or http");
    const prefix = this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix;
    return this.#t.request("POST", `${prefix}/embed`, { text, provider });
  }
  async updateMetadata(recordId, metadata, options) {
    const prefix = this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix;
    const path = `${prefix}/vectors/${id(recordId)}/metadata`;
    const ifMatch = await this.#t.conditional(options);
    return this.#t.request("PATCH", path, { metadata: object(metadata, "metadata") }, { ifMatch });
  }
  groupedSearch(request) { return this.#t.request("POST", `${this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix}/search/grouped`, object(request, "request")); }
  recommend(request) { return this.#t.request("POST", `${this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix}/recommend`, object(request, "request")); }
  facets(field, filter = []) { return this.#t.request("POST", `${this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix}/facets`, { field, filter }); }
  numericStats(field, filter = []) { return this.#t.request("POST", `${this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix}/stats/numeric`, { field, filter }); }
  geoSearch(request) { return this.#t.request("POST", `${this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix}/search/geo`, object(request, "request")); }
  configureVector(name, { dimensions, metric = "squared_l2", activeVersion = "v1", history = 0 }) {
    return this.#t.request("POST", `${this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix}/vectors/configure`, { name, dimensions, metric, active_version: activeVersion, history });
  }
  putNamedVectors(recordId, vectors) { return this.#t.request("PUT", `${this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix}/vectors/${id(recordId)}/named`, object(vectors, "vectors")); }
  searchNamed(request) { return this.#t.request("POST", `${this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix}/search/named`, object(request, "request")); }
  searchSparse(request) { return this.#t.request("POST", `${this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix}/search/sparse`, object(request, "request")); }
  searchDenseSparse(request) { return this.#t.request("POST", `${this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix}/search/dense-sparse`, object(request, "request")); }
  query(query, vectors = {}) { return this.#t.request("POST", `${this.#prefix === "/v1" ? "/v1/collections/default" : this.#prefix}/query`, { query, vectors }); }
  indexStatus() { return this.#t.request("GET", `${this.#prefix}/index/hnsw`); }
  async buildIndex({ metric, quantization, rerankMultiplier } = {}) {
    // Legacy routes keep the historical body ({} for squared_l2); collection routes
    // omit unset fields so the collection's settings apply.
    const body = indexBody(this.#legacy && metric === "squared_l2" ? undefined : metric, quantization, rerankMultiplier);
    await this.#t.request("POST", `${this.#prefix}/index/hnsw`, body);
  }
  rebuildStatus() { return this.#t.request("GET", `${this.#prefix}/index/hnsw/rebuild`); }
  startRebuild(request = {}) {
    return this.#t.request("POST", `${this.#prefix}/index/hnsw/rebuild`, object(request, "request"));
  }
  pollRebuild() { return this.#t.request("POST", `${this.#prefix}/index/hnsw/rebuild/poll`); }
  cancelRebuild() { return this.#t.request("POST", `${this.#prefix}/index/hnsw/rebuild/cancel`); }
  maintenanceStatus() { return this.#t.request("GET", `${this.#prefix}/index/hnsw/maintenance`); }
  maintenanceTick() { return this.#t.request("POST", `${this.#prefix}/index/hnsw/maintenance/tick`); }
}

/** Operations scoped to one named collection; obtain with client.collection(name). */
export class Collection extends Records {
  #t;
  #prefix;
  #client;
  constructor(t, client, name) {
    super(t, `/v1/collections/${collectionName(name)}`, false);
    this.#t = t;
    this.#prefix = `/v1/collections/${name}`;
    this.#client = client;
    this.name = name;
  }
  info() { return this.#client.getCollection(this.name); }
  annQuality(options = {}) {
    return this.#t.request("POST", `${this.#prefix}/index/hnsw/quality`, object(options, "options"));
  }
}

export class Client extends Records {
  #t;
  constructor(baseUrl, token, { timeoutMs = 30000 } = {}) {
    const url = new URL(baseUrl);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash || url.pathname !== "/") {
      throw new TypeError("baseUrl must be an HTTP(S) origin without credentials, path, query or fragment");
    }
    if (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 2147483647) throw new TypeError("Invalid timeoutMs");
    const t = transport(url.origin, token, timeoutMs);
    super(t, "/v1", true);
    this.#t = t;
  }
  health() { return this.#t.request("GET", "/health"); }
  ready() { return this.#t.request("GET", "/ready"); }
  database() { return this.#t.request("GET", "/v1/database"); }
  metrics() { return this.#t.request("GET", "/metrics"); }
  prometheusMetrics() { return this.#t.request("GET", "/metrics?format=prometheus", undefined, { text: true }); }
  capabilities() { return this.#t.request("GET", "/v1/capabilities"); }
  async buildIndex({ metric = "squared_l2", quantization, rerankMultiplier } = {}) {
    await super.buildIndex({ metric, quantization, rerankMultiplier });
  }

  collection(name) { return new Collection(this.#t, this, name); }
  listCollections() { return this.#t.request("GET", "/v1/collections"); }
  createCollection(name, dimensions, settings) {
    const body = { name: collectionName(name), dimensions: integer(dimensions, 1, 4294967295, "dimensions") };
    if (settings !== undefined) body.settings = object(settings, "settings");
    return this.#t.request("POST", "/v1/collections", body);
  }
  getCollection(name) { return this.#t.request("GET", `/v1/collections/${collectionName(name)}`); }
  async dropCollection(name) { return (await this.#t.request("DELETE", `/v1/collections/${collectionName(name)}`)).deleted; }
  setCollectionSettings(name, settings) {
    return this.#t.request("PUT", `/v1/collections/${collectionName(name)}/settings`, object(settings, "settings"));
  }

  beginImport(collection) { return this.#t.request("POST", `/v1/collections/${collectionName(collection)}/imports`); }
  importChunk(job, chunk, operations, { policy: rowPolicy } = {}) {
    const path = `/v1/imports/${id(job)}/chunks/${id(chunk)}`;
    return this.#t.request("PUT", path, { operations, ...(rowPolicy === undefined ? {} : { policy: policy(rowPolicy) }) });
  }
  importStatus(job) { return this.#t.request("GET", `/v1/imports/${id(job)}`); }
  finishImport(job) { return this.#t.request("POST", `/v1/imports/${id(job)}/finish`); }
  abortImport(job) { return this.#t.request("POST", `/v1/imports/${id(job)}/abort`); }
  async importNdjson(path, collection, { chunkSize = 1024, policy: rowPolicy = "reject_chunk", job } = {}) {
    collectionName(collection);
    integer(chunkSize, 1, 4096, "chunkSize");
    policy(rowPolicy);
    if (job !== undefined) id(job);
    // Read and validate the whole file before reserving or resuming a job, so a
    // missing, unreadable, empty or malformed file never orphans a reservation.
    let chunks = 0n;
    for await (const _ of ndjsonChunks(path, chunkSize)) chunks += 1n;
    if (job === undefined && chunks === 0n) throw new TypeError("file contains no operations");
    let status;
    if (job === undefined) {
      status = await this.beginImport(collection);
    } else {
      status = await this.importStatus(job);
      if (status.collection !== collection || status.collection_dropped) throw new TypeError("import job belongs to a different collection");
      if (status.state === "finished") return status;
      if (status.state !== "open") throw new TypeError("import job is not open");
    }
    const current = id(status.job);
    // Later failures keep the job (committed chunks stay resumable) and expose its ID.
    try {
      const last = exactCount(status.last_chunk);
      if (chunks < last) throw new TypeError("file has fewer chunks than the job already committed");
      let count = 0n;
      for await (const rows of ndjsonChunks(path, chunkSize)) {
        count += 1n;
        if (count < last) continue;
        const report = await this.importChunk(current, String(count), rows, { policy: rowPolicy });
        const expected = count === last ? "already_committed" : "committed";
        if (report?.outcome !== expected) {
          throw new TransportError(`import chunk ${count}: expected outcome ${expected}; no retry performed`);
        }
      }
      if (count !== chunks) throw new TypeError("file changed during import");
      return await this.finishImport(current);
    } catch (error) {
      if (typeof error === "object" && error !== null) error.importJob = current;
      throw error;
    }
  }

  changes({ cursor, limit = 1000 } = {}) {
    integer(limit, 1, 10000, "limit");
    const params = new URLSearchParams({ limit: String(limit), ...(cursor === undefined ? {} : { cursor }) });
    return this.#t.request("GET", `/v1/changes?${params}`);
  }
  listWebhooks() { return this.#t.request("GET", "/v1/admin/webhooks"); }
  createWebhook(request) { return this.#t.request("POST", "/v1/admin/webhooks", object(request, "request")); }
  webhookStatus(webhookId) {
    if (typeof webhookId !== "string" || !/^[a-f0-9]{24}$/.test(webhookId)) throw new TypeError("invalid webhook ID");
    return this.#t.request("GET", `/v1/admin/webhooks/${webhookId}`);
  }
  deleteWebhook(webhookId) {
    if (typeof webhookId !== "string" || !/^[a-f0-9]{24}$/.test(webhookId)) throw new TypeError("invalid webhook ID");
    return this.#t.request("DELETE", `/v1/admin/webhooks/${webhookId}`).then((value) => value.deleted);
  }
  autoMaintenance() { return this.#t.request("GET", "/v1/admin/maintenance/auto"); }
  setAutoMaintenance(policyValue) { return this.#t.request("PUT", "/v1/admin/maintenance/auto", object(policyValue, "policy")); }
  tickAutoMaintenance() { return this.#t.request("POST", "/v1/admin/maintenance/auto/tick"); }
  backupSchedule() { return this.#t.request("GET", "/v1/admin/backups/schedule"); }
  setBackupSchedule(schedule) { return this.#t.request("PUT", "/v1/admin/backups/schedule", object(schedule, "schedule")); }
  runBackupSchedule() { return this.#t.request("POST", "/v1/admin/backups/schedule/run"); }
  listBackups() { return this.#t.request("GET", "/v1/admin/backups"); }
  createBackup() { return this.#t.request("POST", "/v1/admin/backups"); }
  verifyBackup(name) { return this.#t.request("POST", `/v1/admin/backups/${backupName(name)}/verify`); }
  drillBackup(name) { return this.#t.request("POST", `/v1/admin/backups/${backupName(name)}/drill`); }
  slowQueries() { return this.#t.request("GET", "/v1/admin/diagnostics/slow-queries"); }

  listCredentials() { return this.#t.request("GET", "/v1/admin/credentials"); }
  createCredential(request) { return this.#t.request("POST", "/v1/admin/credentials", object(request, "request")); }
  rotateCredential(credential, { graceSeconds } = {}) {
    const body = graceSeconds === undefined ? {} : { grace_seconds: integer(graceSeconds, 0, 86400, "graceSeconds") };
    return this.#t.request("POST", `/v1/admin/credentials/${credentialId(credential)}/rotate`, body);
  }
  revokeCredential(credential) { return this.#t.request("POST", `/v1/admin/credentials/${credentialId(credential)}/revoke`); }
  persistentAudit({ limit = 100 } = {}) {
    return this.#t.request("GET", `/v1/admin/audit/persistent?limit=${integer(limit, 1, 1000, "limit")}`);
  }
}
