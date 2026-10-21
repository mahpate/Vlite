"use strict";
(() => {
  const $ = (id) => document.getElementById(id);
  const apiBase = document.querySelector('meta[name="vlite-api-base"]')?.content || "";
  let token = "";
  let connected = false;
  let busy = false;
  let nextAfter = null;
  let pageSize = 25;
  let dimensions = null;
  let capabilities = null;
  let activeRequest = null;
  let tools = null;
  let lastSearch = null;
  function connectionState() {
    $("connection-badge").textContent = connected ? "Connected" : "Disconnected";
    $("connection-badge").dataset.connected = String(connected);
  }
  function staleSnapshot() {
    for (const id of ["stat-records", "stat-bytes"]) $(id).textContent = "--";
    $("stat-index").textContent = "Refresh needed";
    $("snapshot-note").textContent = "Data changed. Connect / refresh updates the overview snapshot.";
  }
  function mutationChanged() {
    staleSnapshot();
    $("index-status").textContent = "Data changed or outcome unknown; refresh index status";
    clearBrowse();
    $("hits").querySelector("tbody").replaceChildren();
    for (const id of ["payload-preview", "vector-slice-result", "vector-summary"]) $(id).textContent = "";
  }
  function clearBrowse() {
    nextAfter = null;
    $("vectors").querySelector("tbody").replaceChildren();
    $("browse-status").textContent = "";
    $("next-page").disabled = true;
  }
  const show = (id, value) => { $(id).textContent = JSON.stringify(value, null, 2); };
  const status = (message, error = false) => {
    $("status").textContent = message;
    $("status").dataset.error = String(error);
    const help = error ? /401/.test(message) ? "Check the token file; rotation/revocation requires reconnecting."
      : /403/.test(message) ? "This credential cannot perform this action. Use a write/admin credential when appropriate."
        : /dimension/i.test(message) ? "Use a vector with the dimension count shown in Overview."
          : /index_not_ready/.test(message) ? "Build the matching HNSW metric, or use exact search."
            : /busy|capacity|503/.test(message) ? "The owner or request queue is busy. Inspect maintenance/health before retrying reads."
              : /unknown|abort|timeout|Network/.test(message) ? "Stopping the browser wait does not cancel server work. Verify mutation outcomes before any manual retry."
                : "Review the request and response inspector. Changes are never automatically retried." : "";
    $("error-help").textContent = help;
  };
  function clearData() {
    for (const id of ["overview", "record", "inspector", "build-result"]) $(id).textContent = "";
    $("hits").querySelector("tbody").replaceChildren();
    $("record-vector").value = "[1, 0, 0]";
    $("payload").value = "";
    $("metadata").value = "{}";
    clearBrowse();
    $("index-status").textContent = "Index readiness not checked";
    dimensions = null;
    lastSearch = null;
    for (const id of ["stat-records", "stat-dimensions", "stat-bytes"]) $(id).textContent = "--";
    $("stat-index").textContent = "Not checked";
    $("snapshot-note").textContent = "Connect to view a database snapshot. Refresh after external writes.";
    $("search-summary").textContent = "Run a search to compare vectors. Lower distance is better.";
  }
  function disconnect() {
    token = "";
    connected = false;
    connectionState();
    activeRequest?.abort();
    capabilities = null;
    tools?.clear();
    $("token").value = "";
    clearData();
    status("Disconnected; token and displayed results cleared");
  }
  function confirmOperation(message) {
    $("confirmation-text").textContent = message;
    const dialog = $("confirmation");
    dialog.returnValue = "cancel";
    return new Promise((resolve) => {
      dialog.addEventListener("close", () => resolve(dialog.returnValue === "confirm"), { once: true });
      dialog.showModal();
    });
  }
  async function run(operation) {
    if (busy) return;
    busy = true;
    document.querySelectorAll("button").forEach((button) => { button.disabled = true; });
    // Dialog controls must remain usable while an operation awaits confirmation.
    $("confirmation").querySelectorAll("button").forEach((button) => { button.disabled = false; });
    try { await operation(); }
    catch (error) { status(error.message, true); }
    finally {
      busy = false;
      document.querySelectorAll("button").forEach((button) => { button.disabled = false; });
      $("next-page").disabled = nextAfter === null || !connected;
      $("cancel-request").disabled = activeRequest === null;
      if (capabilities?.role === "read_only") document.querySelectorAll("[data-admin], [data-write], #delete-vector, #build-index, #vector-form button[type=submit]").forEach((button) => { button.disabled = true; });
    }
  }
  async function api(method, path, body) {
    if (!connected) throw new Error("Connect before accessing the database");
    if (capabilities?.role === "read_only" && method !== "GET" && path !== "/v1/search") throw new Error("HTTP 403: read-only credentials cannot mutate data");
    const started = performance.now();
    const request = { method, path, ...(body === undefined ? {} : { body }) };
    show("inspector", { request, state: "pending" });
    let response;
    let value;
    const controller = new AbortController();
    activeRequest = controller;
    $("cancel-request").disabled = false;
    const timeout = setTimeout(() => controller.abort(), path.includes("/index/") ? 3600000 : 60000);
    try {
      response = await fetch(`${apiBase}${path}`, {
        signal: controller.signal,
        method, cache: "no-store", credentials: "omit", redirect: "error",
        headers: { Authorization: `Bearer ${token}`, ...(body === undefined ? {} : { "Content-Type": "application/json" }) },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
      const text = await response.text();
      value = text ? JSON.parse(text) : null;
    } catch (error) {
      if (method !== "GET" && path !== "/v1/search") mutationChanged();
      show("inspector", { request, error: "Network or response parsing failed", latency_ms: performance.now() - started });
      throw new Error(`${method} ${path} failed; mutation outcome may be unknown. Do not blindly retry.`);
    } finally {
      clearTimeout(timeout);
      if (activeRequest === controller) activeRequest = null;
      $("cancel-request").disabled = true;
    }
    show("inspector", {
      request, status: response.status, request_id: response.headers.get("x-request-id"),
      latency_ms: performance.now() - started, response: value,
    });
    if (!response.ok) {
      if (response.status >= 500 && method !== "GET" && path !== "/v1/search") mutationChanged();
      if (response.status === 401) {
        token = "";
        connected = false;
        connectionState();
        clearData();
        tools?.clear();
        show("inspector", { request, status: response.status, response: value });
        $("token").value = "";
        clearBrowse();
        $("index-status").textContent = "Index readiness not checked";
      }
      const detail = value?.error;
      throw new Error(`HTTP ${response.status}: ${detail?.code || "request_failed"}: ${detail?.message || "See inspector"}`);
    }
    if (method !== "GET" && path !== "/v1/search") {
      mutationChanged();
    }
    status(`${method} ${path}: HTTP ${response.status}`);
    return value;
  }
  const parse = (id) => {
    try { return JSON.parse($(id).value); }
    catch { throw new Error(`${id}: invalid JSON; no request sent`); }
  };
  const vector = (id) => {
    const value = parse(id);
    if (!Array.isArray(value) || !value.length || !value.every((v) => typeof v === "number" && Number.isFinite(v))) {
      throw new Error(`${id}: expected a nonempty finite numeric array`);
    }
    return value;
  };
  const recordPath = () => {
    const id = $("id").value;
    if (!/^(0|[1-9][0-9]*)$/.test(id) || id.length > 20 || BigInt(id) > 18446744073709551615n) {
      throw new Error("ID must be a canonical decimal u64");
    }
    return `/v1/vectors/${id}`;
  };
  function indexConfig() {
    const m = Number($("hnsw-m").value), ef = Number($("hnsw-ef").value), seed = $("hnsw-seed").value;
    if (!Number.isInteger(m) || m < 2 || m > 64 || !Number.isInteger(ef) || ef < m || ef > 4096) throw new Error("HNSW requires m 2..64 and ef_construction m..4096");
    window.VLiteTools.utils.id(seed);
    return { m, ef_construction: ef, seed };
  }
  function modeChanged() {
    const exact = $("mode").value === "exact";
    $("ef").disabled = exact;
    $("filter").disabled = !exact;
    const innerProduct = $("metric").querySelector('option[value="inner_product"]');
    innerProduct.disabled = !exact;
    if (!exact && $("metric").value === "inner_product") $("metric").value = "squared_l2";
  }
  $("mode").addEventListener("change", modeChanged);
  async function refreshIndex() {
    const result = await api("GET", "/v1/index/hnsw");
    $("index-status").textContent = `${result.ready ? "Ready" : "Not ready: explicit build required for ready-only search"}; metric ${result.metric ?? "none"}; buffer ${result.buffered_ids}/${result.max_buffer_ids} IDs, ${result.buffer_vector_bytes}/${result.max_buffer_vector_bytes} vector bytes (snapshot)`;
    $("stat-index").textContent = result.ready ? "Ready" : "Not ready";
  }
  async function loadRecord(path) {
    $("record").textContent = "";
    const result = await api("GET", path);
    show("record", result);
    $("id").value = result.id;
    $("record-vector").value = JSON.stringify(result.vector);
    $("payload").value = result.payload_base64;
    $("metadata").value = JSON.stringify(result.metadata, null, 2);
    tools?.loaded(result);
  }
  async function inspectRecord(path) {
    await loadRecord(path);
    $("editor").scrollIntoView({ block: "start" });
    $("id").focus({ preventScroll: true });
  }
  async function browse(after) {
    const requestedSize = after === null ? Number($("page-size").value) : pageSize;
    if (!Number.isInteger(requestedSize) || requestedSize < 1 || requestedSize > 100) throw new Error("Page size must be 1..100");
    // Invalidate stale continuation on failure; never silently skip a page.
    clearBrowse();
    const result = await api("GET", `/v1/vectors?limit=${requestedSize}${after === null ? "" : `&after=${after}`}`);
    pageSize = requestedSize;
    nextAfter = result.next_after;
    const table = $("vectors").querySelector("tbody");
    for (const item of result.items) {
      const row = document.createElement("tr");
      const idCell = document.createElement("td");
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = item.id;
      button.addEventListener("click", () => run(() => inspectRecord(`/v1/vectors/${item.id}`)));
      idCell.append(button);
      const metadata = document.createElement("td");
      metadata.textContent = JSON.stringify(item.metadata);
      row.append(idCell, metadata);
      table.append(row);
    }
    $("browse-status").textContent = `${result.items.length} records; ${nextAfter === null ? "end of current list" : "more records available"}. Live view: concurrent changes may affect subsequent pages.`;
  }
  $("browse-form").addEventListener("submit", (event) => {
    event.preventDefault();
    run(() => browse(null));
  });
  $("next-page").addEventListener("click", () => {
    if (nextAfter !== null) run(() => browse(nextAfter));
  });
  $("refresh-index").addEventListener("click", () => run(refreshIndex));
  $("connection").addEventListener("submit", (event) => {
    event.preventDefault();
    run(async () => {
      token = $("token").value || token;
      $("token").value = "";
      clearData();
      tools?.clear();
      connected = true;
      capabilities = null;
      const database = await api("GET", "/v1/database");
      const metrics = await api("GET", "/metrics");
      show("overview", { database, metrics });
      dimensions = database.dimensions;
      $("stat-records").textContent = String(database.records);
      $("stat-dimensions").textContent = String(database.dimensions);
      $("stat-bytes").textContent = String(metrics.database_bytes ?? "Unavailable");
      $("snapshot-note").textContent = "Snapshot from the last connection refresh. External writes may change these values.";
      await refreshIndex();
      capabilities = await api("GET", "/v1/capabilities");
      if (capabilities.api_version !== 1 || !["admin", "read_only"].includes(capabilities.role)) {
        disconnect();
        throw new Error("Unsupported server API version or role; use a matching console");
      }
      show("compatibility", capabilities);
      tools.connected(capabilities);
      $("mode").querySelector('option[value="approximate"]').disabled = capabilities.role === "read_only";
      if (capabilities.role === "read_only" && $("mode").value === "approximate") {
        $("mode").value = "exact";
        modeChanged();
      }
      connectionState();
      status("Connected; overview refreshed");
    });
  });
  $("disconnect").addEventListener("click", disconnect);
  $("cancel-request").addEventListener("click", () => activeRequest?.abort());
  $("example-record").addEventListener("click", () => run(async () => {
    if (!connected || dimensions === null) throw new Error("Connect before filling a dimension-matched example");
    if (dimensions > 4096) throw new Error("Example generation is limited to 4096 dimensions; enter your vector manually");
    const example = Array(dimensions).fill(0);
    example[0] = 1;
    $("record-vector").value = JSON.stringify(example);
    $("metadata").value = JSON.stringify({ category: { type: "string", value: "demo" } }, null, 2);
    $("payload").value = "";
    status("Example filled; nothing saved. Choose an ID and Put / replace to commit.");
  }));
  $("use-record-query").addEventListener("click", () => run(async () => {
    $("query").value = JSON.stringify(vector("record-vector"));
    status("Editor vector copied to query; choose a mode and Search.");
  }));
  $("get-vector").addEventListener("click", () => run(() => loadRecord(recordPath())));
  $("vector-form").addEventListener("submit", (event) => {
    event.preventDefault();
    run(async () => {
      const path = recordPath();
      const metadata = parse("metadata");
      if (!metadata || Array.isArray(metadata) || typeof metadata !== "object") throw new Error("Metadata must be a typed JSON object");
      const body = { vector: vector("record-vector"), payload_base64: $("payload").value, metadata };
      if (!await confirmOperation(`Replace ${path}? The vector, payload and entire metadata map will be durably replaced.`)) return;
      await api("PUT", path, body);
      $("hits").querySelector("tbody").replaceChildren();
      show("record", { committed: true, id: path.split("/").pop() });
      clearBrowse();
      $("index-status").textContent = "Data changed; refresh index status";
      staleSnapshot();
    });
  });
  $("delete-vector").addEventListener("click", () => run(async () => {
    const path = recordPath();
    if (!await confirmOperation(`Durably delete ${path}? This cannot be undone by the dashboard.`)) return;
    show("record", await api("DELETE", path));
    $("hits").querySelector("tbody").replaceChildren();
    clearBrowse();
    $("index-status").textContent = "Data changed; refresh index status";
    staleSnapshot();
  }));
  async function search() {
      const table = $("hits").querySelector("tbody");
      table.replaceChildren();
      $("search-summary").textContent = "Search not completed";
      const mode = $("mode").value;
      const body = { vector: vector("query"), k: Number($("k").value), mode, metric: $("metric").value, config: indexConfig() };
      if (!Number.isInteger(body.k) || body.k < 0 || body.k > 10000) throw new Error("k must be 0..10000");
      if (mode === "exact") {
        body.filter = parse("filter");
        if (!Array.isArray(body.filter)) throw new Error("Filter must be a JSON array");
      } else {
        body.ef_search = Number($("ef").value);
        if (!Number.isInteger(body.ef_search) || body.ef_search < 1 || body.ef_search > 100000) throw new Error("ef must be 1..100000");
      }
      const started = performance.now();
      const result = await api("POST", "/v1/search", body);
      lastSearch = { body, result };
      $("search-summary").textContent = `${result.hits.length} neighbors returned in ${(performance.now() - started).toFixed(1)} ms (browser-observed); ${body.mode}, ${body.metric}. Select an ID to inspect.`;
      $("hits").querySelector("caption").textContent = `Search results (${body.metric ?? "squared_l2"}; lower distance is better)`;
      for (const hit of result.hits) {
        const row = document.createElement("tr");
        const idCell = document.createElement("td");
        const button = document.createElement("button");
        button.type = "button";
        button.textContent = hit.id;
        button.addEventListener("click", () => run(() => inspectRecord(`/v1/vectors/${hit.id}`)));
        idCell.append(button);
        const distance = document.createElement("td");
        distance.textContent = String(hit.distance);
        row.append(idCell, distance);
        table.append(row);
      }
  }
  $("search-form").addEventListener("submit", (event) => {
    event.preventDefault();
    run(search);
  });
  $("build-index").addEventListener("click", () => run(async () => {
    if (!await confirmOperation("Build HNSW now? This synchronously blocks the single database owner and clears the mutation buffer.")) return;
    $("build-result").textContent = "Building; no automatic retry";
    const started = performance.now();
    try {
      if ($("metric").value === "inner_product") throw new Error("HNSW supports squared L2 and cosine, not inner product");
      await api("POST", "/v1/index/hnsw", { metric: $("metric").value, config: indexConfig() });
      $("build-result").textContent = `Build completed in ${(performance.now() - started).toFixed(1)} ms (browser-observed)`;
      await refreshIndex();
    } catch (error) {
      $("build-result").textContent = "Build failed or outcome unknown; inspect the response";
      throw error;
    }
  }));
  tools = window.VLiteTools.init({
    api, run, status, confirm: confirmOperation, inspect: inspectRecord, search, indexConfig, disconnect,
    dimensions: () => dimensions, busy: () => busy, connected: () => connected,
    capabilities: () => capabilities, lastSearch: () => lastSearch,
  });
  window.addEventListener("pagehide", disconnect);
})();
