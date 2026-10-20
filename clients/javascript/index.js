function id(value) {
  if (typeof value !== "string" || !/^(0|[1-9][0-9]{0,19})$/.test(value) || BigInt(value) > 18446744073709551615n) {
    throw new TypeError("ID must be a canonical decimal u64 string");
  }
  return value;
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

export class TransportError extends Error {
  constructor() {
    super("Request failed or response was invalid; mutation outcome may be unknown; no retry performed");
    this.name = "TransportError";
  }
}

export class Client {
  #base;
  #token;
  #timeout;
  constructor(baseUrl, token, { timeoutMs = 30000 } = {}) {
    const url = new URL(baseUrl);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash || url.pathname !== "/") {
      throw new TypeError("baseUrl must be an HTTP(S) origin without credentials, path, query or fragment");
    }
    if (typeof token !== "string" || !/^[A-Za-z0-9_-]{32,256}$/.test(token)) throw new TypeError("Invalid bearer token");
    if (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 2147483647) throw new TypeError("Invalid timeoutMs");
    this.#base = url.origin;
    this.#token = token;
    this.#timeout = timeoutMs;
  }
  async #request(method, path, body) {
    // Reject nonfinite numbers rather than JSON.stringify silently converting them to null.
    const encoded = body === undefined ? undefined : JSON.stringify(body, (_, value) => {
      if (typeof value === "number" && !Number.isFinite(value)) throw new TypeError("Numbers must be finite");
      return value;
    });
    let response, value;
    try {
      response = await fetch(this.#base + path, {
        method, body: encoded, redirect: "error", credentials: "omit", cache: "no-store",
        signal: AbortSignal.timeout(this.#timeout),
        headers: { Authorization: `Bearer ${this.#token}`, Accept: "application/json",
          ...(encoded === undefined ? {} : { "Content-Type": "application/json" }) },
      });
      const raw = await response.text();
      try { value = raw ? JSON.parse(raw) : null; }
      catch { if (response.ok) throw new TransportError(); }
    } catch { throw new TransportError(); }
    if (!response.ok) {
      const detail = value?.error;
      throw new VLiteError(response.status, detail?.code ?? "http_error",
        detail?.message ?? "Server returned a nonstandard error response", response.headers.get("x-request-id"));
    }
    return value;
  }
  health() { return this.#request("GET", "/health"); }
  ready() { return this.#request("GET", "/ready"); }
  database() { return this.#request("GET", "/v1/database"); }
  metrics() { return this.#request("GET", "/metrics"); }
  get(recordId) { return this.#request("GET", `/v1/vectors/${id(recordId)}`); }
  async put(recordId, record) { await this.#request("PUT", `/v1/vectors/${id(recordId)}`, record); }
  async delete(recordId) { return (await this.#request("DELETE", `/v1/vectors/${id(recordId)}`)).deleted; }
  listVectors({ limit = 25, after } = {}) {
    if (!Number.isInteger(limit) || limit < 1 || limit > 100) throw new TypeError("limit must be 1..100");
    return this.#request("GET", `/v1/vectors?limit=${limit}${after === undefined ? "" : `&after=${id(after)}`}`);
  }
  async batch(operations) {
    for (const operation of operations) id(operation.id);
    await this.#request("POST", "/v1/batches", { operations });
  }
  async search(request) { return (await this.#request("POST", "/v1/search", request)).hits; }
  indexStatus() { return this.#request("GET", "/v1/index/hnsw"); }
  async buildIndex({ metric = "squared_l2" } = {}) {
    await this.#request("POST", "/v1/index/hnsw", metric === "squared_l2" ? {} : { metric });
  }
}
