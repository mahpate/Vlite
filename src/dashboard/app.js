"use strict";
(() => {
  const $ = (id) => document.getElementById(id);
  const apiBase = document.querySelector('meta[name="vlite-api-base"]')?.content || "";
  let token = "";
  let connected = false;
  let busy = false;
  let nextAfter = null;
  let pageSize = 25;
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
  };
  function clearData() {
    for (const id of ["overview", "record", "inspector", "build-result"]) $(id).textContent = "";
    $("hits").querySelector("tbody").replaceChildren();
    $("record-vector").value = "[1, 0, 0]";
    $("payload").value = "";
    $("metadata").value = "{}";
    clearBrowse();
    $("index-status").textContent = "Index readiness not checked";
  }
  function disconnect() {
    token = "";
    connected = false;
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
    }
  }
  async function api(method, path, body) {
    if (!connected) throw new Error("Connect before accessing the database");
    const started = performance.now();
    const request = { method, path, ...(body === undefined ? {} : { body }) };
    show("inspector", { request, state: "pending" });
    let response;
    let value;
    try {
      response = await fetch(`${apiBase}${path}`, {
        method, cache: "no-store", credentials: "omit", redirect: "error",
        headers: { Authorization: `Bearer ${token}`, ...(body === undefined ? {} : { "Content-Type": "application/json" }) },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
      const text = await response.text();
      value = text ? JSON.parse(text) : null;
    } catch (error) {
      show("inspector", { request, error: "Network or response parsing failed", latency_ms: performance.now() - started });
      throw new Error(`${method} ${path} failed; mutation outcome may be unknown. Do not blindly retry.`);
    }
    show("inspector", {
      request, status: response.status, request_id: response.headers.get("x-request-id"),
      latency_ms: performance.now() - started, response: value,
    });
    if (!response.ok) {
      if (response.status === 401) {
        token = "";
        connected = false;
        $("token").value = "";
        clearBrowse();
        $("index-status").textContent = "Index readiness not checked";
      }
      const detail = value?.error;
      throw new Error(`HTTP ${response.status}: ${detail?.code || "request_failed"}: ${detail?.message || "See inspector"}`);
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
  }
  async function loadRecord(path) {
    $("record").textContent = "";
    const result = await api("GET", path);
    show("record", result);
    $("id").value = result.id;
    $("record-vector").value = JSON.stringify(result.vector);
    $("payload").value = result.payload_base64;
    $("metadata").value = JSON.stringify(result.metadata, null, 2);
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
      button.addEventListener("click", () => run(() => loadRecord(`/v1/vectors/${item.id}`)));
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
      connected = true;
      const database = await api("GET", "/v1/database");
      const metrics = await api("GET", "/metrics");
      show("overview", { database, metrics });
      await refreshIndex();
      status("Connected; overview refreshed");
    });
  });
  $("disconnect").addEventListener("click", disconnect);
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
    });
  });
  $("delete-vector").addEventListener("click", () => run(async () => {
    const path = recordPath();
    if (!await confirmOperation(`Durably delete ${path}? This cannot be undone by the dashboard.`)) return;
    show("record", await api("DELETE", path));
    $("hits").querySelector("tbody").replaceChildren();
    clearBrowse();
    $("index-status").textContent = "Data changed; refresh index status";
  }));
  $("search-form").addEventListener("submit", (event) => {
    event.preventDefault();
    run(async () => {
      const table = $("hits").querySelector("tbody");
      table.replaceChildren();
      const mode = $("mode").value;
      const body = { vector: vector("query"), k: Number($("k").value), mode, metric: $("metric").value };
      if (!Number.isInteger(body.k) || body.k < 0 || body.k > 10000) throw new Error("k must be 0..10000");
      if (mode === "exact") {
        body.filter = parse("filter");
        if (!Array.isArray(body.filter)) throw new Error("Filter must be a JSON array");
      } else {
        body.ef_search = Number($("ef").value);
        if (!Number.isInteger(body.ef_search) || body.ef_search < 1 || body.ef_search > 100000) throw new Error("ef must be 1..100000");
      }
      const result = await api("POST", "/v1/search", body);
      $("hits").querySelector("caption").textContent = `Search results (${body.metric ?? "squared_l2"}; lower distance is better)`;
      for (const hit of result.hits) {
        const row = document.createElement("tr");
        for (const value of [hit.id, hit.distance]) {
          const cell = document.createElement("td");
          cell.textContent = String(value);
          row.append(cell);
        }
        table.append(row);
      }
    });
  });
  $("build-index").addEventListener("click", () => run(async () => {
    if (!await confirmOperation("Build HNSW now? This synchronously blocks the single database owner and clears the mutation buffer.")) return;
    $("build-result").textContent = "Building; no automatic retry";
    const started = performance.now();
    try {
      if ($("metric").value === "inner_product") throw new Error("HNSW supports squared L2 and cosine, not inner product");
      await api("POST", "/v1/index/hnsw", { metric: $("metric").value });
      $("build-result").textContent = `Build completed in ${(performance.now() - started).toFixed(1)} ms (browser-observed)`;
      await refreshIndex();
    } catch (error) {
      $("build-result").textContent = "Build failed or outcome unknown; inspect the response";
      throw error;
    }
  }));
  window.addEventListener("pagehide", disconnect);
})();
