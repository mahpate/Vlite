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
  let browseItems = [];
  // Active collection context. Every record/search/index route is derived from it at
  // request time; switching resets all collection-bound state (see switchCollection).
  let collection = "default";
  let collectionItems = [];
  let editorDirty = false;
  let loaded = null; // { collection, id, revision, etag } of the record last fetched into the editor
  const utils = () => window.VLiteTools.utils;
  const path = (suffix) => utils().collectionPath(collection, suffix);
  const isSearchPath = (value) => utils().isSearch(value);
  const feature = (name) => capabilities?.features?.[name] === true;
  // Newer servers report format_version; older servers have none of the EPIC-015 routes.
  const extended = () => capabilities !== null && capabilities.format_version !== undefined;
  // Settings-sensitive routes (search, index status/build/rebuild/maintenance/quality). Legacy routes apply
  // historical hard defaults even for "default", so newer servers always get the collection route, which uses the
  // selected collection's stored settings (metric, HNSW, quantization, planner, text fields). Old servers: legacy.
  const settingsPath = (suffix) => extended() ? `/v1/collections/${utils().collectionName(collection)}${suffix}` : path(suffix);
  function can(operation, target = collection) {
    if (!capabilities) return false;
    if (capabilities.role === "admin") return true;
    if (capabilities.role === "read_only") return ["read", "search"].includes(operation);
    const scope = capabilities.scope;
    if (capabilities.role !== "scoped" || !scope) return false;
    return scope.operations.includes(operation) && (scope.collections.includes("*") || scope.collections.includes(target));
  }
  function applyRoleGates() {
    if (!capabilities) return;
    const admin = capabilities.role === "admin";
    document.querySelectorAll("[data-admin]").forEach((element) => { if (!admin) element.disabled = true; });
    document.querySelectorAll("[data-write], #delete-vector, #vector-form button[type=submit]").forEach((element) => { if (!can("write")) element.disabled = true; });
    document.querySelectorAll("#build-index, [data-op]").forEach((element) => {
      const operation = element.dataset.op || "index";
      if (!can(operation)) element.disabled = true;
    });
    document.querySelectorAll("[data-feature]").forEach((element) => {
      if (!element.dataset.feature.split(" ").every(feature)) element.disabled = true;
    });
  }
  function collectionState() {
    $("collection-badge").textContent = `Collection: ${collection}`;
    $("collection-note").textContent = collection === "default"
      ? "Default collection (legacy routes). Every browse, edit, search, import and index action applies only to this collection."
      : `Collection "${collection}" (/v1/collections/${collection}/...). Every browse, edit, search, import and index action applies only to this collection.`;
  }
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
    browseItems = [];
    nextAfter = null;
    $("vectors").querySelector("tbody").replaceChildren();
    $("browse-preview").textContent = "";
    $("browse-columns").replaceChildren();
    $("browse-sort").replaceChildren(new Option("ID", ""));
    $("browse-status").textContent = "";
    $("next-page").disabled = true;
  }
  const show = (id, value) => { $(id).textContent = JSON.stringify(value, null, 2); };
  const status = (message, error = false) => {
    $("status").textContent = message;
    $("status").dataset.error = String(error);
    const help = error ? /401/.test(message) ? "Check the token; expired, rotated or revoked credentials require reconnecting with a current token."
      : /forbidden_scope/.test(message) ? "This scoped credential does not cover this collection or operation (or its collection/database was recreated). Ask an admin for a matching credential."
      : /403/.test(message) ? "This credential cannot perform this action. Use a write/admin credential when appropriate."
        : /precondition_failed|412/.test(message) ? "The record changed since it was loaded (or was deleted). Get it again to see the current revision, then reapply your edit."
        : /requires_format_v4/.test(message) ? "Collections and resumable imports need a format v4 file. Upgrade offline with vlite-maintain upgrade SOURCE DEST (a new file; the source is never modified) and restart the server on it."
        : /collection_not_found/.test(message) ? "The collection no longer exists. Reconnect to refresh the collection list."
        : /credentials_commit_indeterminate/.test(message) ? "The credential change IS in effect but may not survive a crash; no secret was returned. Re-list credentials, then revoke or re-issue. Do not assume nothing changed."
        : /import_conflict/.test(message) ? "The server rejected this chunk as inconsistent: an earlier import reservation may still need its first chunk (the message names it), or the job's committed chunks differ. Nothing was written; commit or abort the earlier job, or refresh this job's status and select the original file and chunk size."
        : /dimension/i.test(message) ? "Use a vector with the dimension count of the selected collection."
          : /index_not_ready/.test(message) ? "Build the matching HNSW metric, or use exact search."
            : /busy|capacity|503/.test(message) ? "The owner or request queue is busy. Inspect maintenance/health before retrying reads."
              : /unknown|abort|timeout|Network/.test(message) ? "Stopping the browser wait does not cancel server work. Verify mutation outcomes before any manual retry."
                : "Review the request and response inspector. Changes are never automatically retried." : "";
    $("error-help").textContent = help;
  };
  // Dimension-matched example text for the record/query fields; "" until the selected collection's dimensions are known.
  let exampleText = "[1, 0, 0]";
  const exampleVector = (count) => {
    if (!Number.isInteger(count) || count < 1 || count > 4096) return "";
    const vector = Array(count).fill(0);
    vector[0] = 1;
    return JSON.stringify(vector);
  };
  function resetCollectionData() {
    $("hits").querySelector("tbody").replaceChildren();
    for (const id of ["record", "build-result", "search-plan", "record-revision"]) $(id).textContent = "";
    $("id").value = "1";
    // Never leave a vector sized for the previous collection; showCollectionStats refills for the new dimensions.
    exampleText = "";
    $("record-vector").value = "";
    $("payload").value = "";
    $("metadata").value = "{}";
    $("record-ttl").value = "";
    $("rerank-mode").value = "";
    $("query").value = "";
    $("filter").value = "[]";
    $("search-text").value = "";
    clearBrowse();
    $("index-status").textContent = "Index readiness not checked";
    loaded = null;
    editorDirty = false;
    lastSearch = null;
    $("search-summary").textContent = "Run a search to compare vectors. Lower distance is better.";
  }
  function clearData() {
    for (const id of ["overview", "inspector"]) $(id).textContent = "";
    resetCollectionData();
    dimensions = null;
    collection = "default";
    collectionItems = [];
    $("collection").replaceChildren(new Option("default", "default"));
    collectionState();
    for (const id of ["stat-records", "stat-dimensions", "stat-bytes"]) $(id).textContent = "--";
    $("stat-index").textContent = "Not checked";
    $("encryption-badge").textContent = "Encryption: not checked";
    $("snapshot-note").textContent = "Connect to view a database snapshot. Refresh after external writes.";
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
    $("collection").disabled = true;
    // Dialog controls must remain usable while an operation awaits confirmation.
    $("confirmation").querySelectorAll("button").forEach((button) => { button.disabled = false; });
    // A running resumable import can be paused between chunks.
    $("job-stop").disabled = false;
    try { await operation(); }
    catch (error) { status(error.message, true); }
    finally {
      busy = false;
      document.querySelectorAll("button").forEach((button) => { button.disabled = false; });
      $("collection").disabled = !connected || !feature("collections");
      $("next-page").disabled = nextAfter === null || !connected;
      $("cancel-request").disabled = activeRequest === null;
      tools?.gate();
      applyRoleGates();
    }
  }
  // options: { headers, withHeaders (returns { value, etag, status }), data: false for
  // requests that never change records, sensitive (response holds a one-time secret) }.
  async function api(method, path, body, options = {}) {
    if (!connected) throw new Error("Connect before accessing the database");
    const search = isSearchPath(path);
    if (capabilities?.role === "read_only" && method !== "GET" && !search) throw new Error("HTTP 403: read-only credentials cannot mutate data");
    const changesData = method !== "GET" && !search && options.data !== false;
    const started = performance.now();
    const redact = utils().redact;
    const request = redact({ method, path, ...(options.headers ? { headers: options.headers } : {}), ...(body === undefined ? {} : { body }) });
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
        headers: { Authorization: `Bearer ${token}`, ...(body === undefined ? {} : { "Content-Type": "application/json" }), ...(options.headers || {}) },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
      const text = await response.text();
      value = options.text && response.ok ? text : text ? JSON.parse(text) : null;
    } catch (error) {
      if (changesData) mutationChanged();
      show("inspector", { request, error: "Network or response parsing failed", latency_ms: performance.now() - started });
      throw new Error(`${method} ${path} failed; mutation outcome may be unknown. Do not blindly retry.`);
    } finally {
      clearTimeout(timeout);
      if (activeRequest === controller) activeRequest = null;
      $("cancel-request").disabled = true;
    }
    // Inspector output is always redacted; one-time secrets never reach it.
    show("inspector", {
      request, status: response.status, request_id: response.headers.get("x-request-id"),
      latency_ms: performance.now() - started,
      response: options.sensitive ? "[response withheld: contains a one-time credential secret]" : options.text && response.ok ? `[${value.length} characters of text]` : redact(value),
    });
    if (!response.ok) {
      if (response.status >= 500 && changesData) mutationChanged();
      if (response.status === 401) {
        token = "";
        connected = false;
        connectionState();
        clearData();
        tools?.clear();
        show("inspector", { request, status: response.status, response: redact(value) });
        $("token").value = "";
        clearBrowse();
        $("index-status").textContent = "Index readiness not checked";
      }
      const detail = value?.error;
      const failure = new Error(redact(`HTTP ${response.status}: ${detail?.code || "request_failed"}: ${detail?.message || "See inspector"}`));
      failure.status = response.status;
      failure.code = detail?.code;
      failure.body = options.sensitive ? null : value;
      throw failure;
    }
    if (changesData) {
      mutationChanged();
    }
    status(`${method} ${path}: HTTP ${response.status}`);
    return options.withHeaders ? { value, etag: response.headers.get("etag"), status: response.status } : value;
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
    if (dimensions !== null && value.length !== dimensions) {
      throw new Error(`${id}: has ${value.length} dimensions but collection ${collection} uses ${dimensions}; nothing was sent`);
    }
    return value;
  };
  const recordPath = () => {
    const id = $("id").value;
    if (!/^(0|[1-9][0-9]*)$/.test(id) || id.length > 20 || BigInt(id) > 18446744073709551615n) {
      throw new Error("ID must be a canonical decimal u64");
    }
    return path(`/vectors/${id}`);
  };
  function indexConfig() {
    const m = Number($("hnsw-m").value), ef = Number($("hnsw-ef").value), seed = $("hnsw-seed").value;
    if (!Number.isInteger(m) || m < 2 || m > 64 || !Number.isInteger(ef) || ef < m || ef > 4096) throw new Error("HNSW requires m 2..64 and ef_construction m..4096");
    window.VLiteTools.utils.id(seed);
    return { m, ef_construction: ef, seed };
  }
  // Quantization fields are only sent to servers that advertise them.
  function annOptions() {
    if (!feature("quantization")) return {};
    return { quantization: $("quantization").value, rerank_multiplier: utils().boundedInteger($("rerank-multiplier").value, 1, 16, "Rerank multiplier") };
  }
  function modeChanged() {
    const mode = $("mode").value;
    const hybrid = mode === "hybrid";
    const ann = ["approximate", "approximate_ready"].includes(mode) || (hybrid && $("vector-leg").value === "approximate_ready");
    $("ef").disabled = !(ann || mode === "auto");
    for (const id of ["search-text", "vector-leg", "rrf-k", "vector-weight", "text-weight", "fusion-candidates"]) $(id).disabled = !hybrid;
    $("quantization").disabled = !(ann || mode === "auto") || !feature("quantization");
    $("rerank-multiplier").disabled = $("quantization").disabled;
    const innerProduct = $("metric").querySelector('option[value="inner_product"]');
    innerProduct.disabled = ann;
    if (ann && $("metric").value === "inner_product") $("metric").value = "squared_l2";
    $("mode-help").textContent = {
      exact: "Exact: scans every matching record. Distances; lower is better.",
      approximate: "Approximate: may synchronously build an index (admin or search_build). Distances; lower is better.",
      approximate_ready: "Ready-only ANN: fails with index_not_ready instead of building. Distances; lower is better.",
      auto: "Auto: the server planner picks exact scan, exact over indexed candidates, or ready (filtered) ANN. It never builds an index. Enable Explain to see the chosen plan. Distances; lower is better.",
      hybrid: "Hybrid: BM25 over declared text fields fused with the vector leg by weighted Reciprocal Rank Fusion. Scores; HIGHER is better. Distance/BM25/ranks are shown per leg when present.",
    }[mode];
  }
  $("mode").addEventListener("change", modeChanged);
  $("vector-leg").addEventListener("change", modeChanged);
  // align: on connect/collection switch, query the index that is actually ready. A published index may have been
  // built with an explicit identity that differs from the stored settings; prefilling only the settings would make
  // ready-only/auto searches fail with index_not_ready. The difference is stated, never hidden.
  async function refreshIndex(align = false) {
    const result = await api("GET", settingsPath("/index/hnsw"));
    let note = "";
    if (align && result.ready && result.metric && result.config?.m !== undefined && result.config.ef_construction !== undefined && result.config.seed !== undefined) {
      const differs = $("metric").value !== result.metric || $("hnsw-m").value !== String(result.config.m)
        || $("hnsw-ef").value !== String(result.config.ef_construction) || $("hnsw-seed").value !== String(result.config.seed)
        || (result.quantization !== undefined && ($("quantization").value !== result.quantization || $("rerank-multiplier").value !== String(result.rerank_multiplier)));
      if (differs) {
        $("metric").value = result.metric;
        $("hnsw-m").value = String(result.config.m);
        $("hnsw-ef").value = String(result.config.ef_construction);
        $("hnsw-seed").value = String(result.config.seed);
        if (result.quantization !== undefined) {
          $("quantization").value = result.quantization;
          $("rerank-multiplier").value = String(result.rerank_multiplier);
        }
        modeChanged();
        note = "; form set to the ready index's identity (differs from the stored settings)";
      }
    }
    // Newer servers return a superset of the legacy shape (quantization, rerank, memory).
    const identity = result.quantization !== undefined ? `; quantization ${result.quantization}, rerank x${result.rerank_multiplier}; collection ${collection}` : "";
    $("index-status").textContent = `${result.ready ? "Ready" : "Not ready: explicit build required for ready-only search"}; metric ${result.metric ?? "none"}; buffer ${result.buffered_ids}/${result.max_buffer_ids} IDs, ${result.buffer_vector_bytes}/${result.max_buffer_vector_bytes} vector bytes (snapshot)${identity}${note}`;
    $("stat-index").textContent = result.ready ? "Ready" : "Not ready";
  }
  async function loadRecord(recordPath) {
    $("record").textContent = "";
    $("record-revision").textContent = "";
    const owner = collection;
    const { value: result, etag } = await api("GET", recordPath, undefined, { withHeaders: true });
    if (owner !== collection) throw new Error("Collection changed while loading; record discarded");
    show("record", result);
    $("id").value = result.id;
    $("record-vector").value = JSON.stringify(result.vector);
    $("payload").value = result.payload_base64;
    $("metadata").value = JSON.stringify(result.metadata, null, 2);
    $("record-ttl").value = "";
    loaded = { collection: owner, id: result.id, revision: result.revision ?? null, etag };
    editorDirty = false;
    $("record-revision").textContent = `Loaded from collection ${owner}${result.revision !== undefined ? `, revision ${result.revision}` : ""}${etag ? ". Saving with \"Only if unchanged\" sends If-Match with this record's tag." : ". No ETag returned; writes are unconditional."}`;
    tools?.loaded(result);
    show("browse-preview", { id: result.id, dimensions: result.vector.length, vector_first_32: result.vector.slice(0, 32), metadata: result.metadata });
    $("browse-preview").textContent += `\nPayload preview:\n${$("payload-preview").textContent.slice(0, 4096)}`;
  }
  for (const id of ["id", "record-vector", "payload", "metadata", "record-ttl"]) $(id).addEventListener("input", () => { editorDirty = true; });
  async function inspectRecord(recordPath) {
    await loadRecord(recordPath);
    $("editor").scrollIntoView({ block: "start" });
    $("id").focus({ preventScroll: true });
  }
  async function browse(after) {
    const requestedSize = after === null ? Number($("page-size").value) : pageSize;
    if (!Number.isInteger(requestedSize) || requestedSize < 1 || requestedSize > 100) throw new Error("Page size must be 1..100");
    // Invalidate stale continuation on failure; never silently skip a page.
    clearBrowse();
    const result = await api("GET", path(`/vectors?limit=${requestedSize}${after === null ? "" : `&after=${after}`}`));
    pageSize = requestedSize;
    nextAfter = result.next_after;
    browseItems = result.items;
    const fields = [...new Set(browseItems.flatMap((item) => Object.keys(item.metadata)))].sort();
    for (const field of fields) {
      $("browse-columns").append(new Option(field, field));
      $("browse-sort").append(new Option(field, field));
    }
    renderBrowse();
    $("browse-status").textContent = `${result.items.length} records; ${nextAfter === null ? "end of current list" : "more records available"}. Live view: concurrent changes may affect subsequent pages.`;
  }
  function renderBrowse() {
    const table = $("vectors").querySelector("tbody");
    table.replaceChildren();
    const columns = [...$("browse-columns").selectedOptions].map((option) => option.value);
    const header = $("vectors").querySelector("thead tr");
    header.replaceChildren();
    for (const title of ["ID (select to inspect)", "Typed metadata", ...columns]) {
      const cell = document.createElement("th"); cell.textContent = title; header.append(cell);
    }
    const field = $("browse-sort").value;
    const direction = $("browse-direction").value === "descending" ? -1 : 1;
    const ordered = [...browseItems].sort((a, b) => {
      if (!field) return direction * (BigInt(a.id) < BigInt(b.id) ? -1 : BigInt(a.id) > BigInt(b.id) ? 1 : 0);
      const left = a.metadata[field], right = b.metadata[field];
      if (!left || !right) return left ? -1 : right ? 1 : 0;
      const x = left.type === "integer" ? BigInt(left.value) : left.value;
      const y = right.type === "integer" ? BigInt(right.value) : right.value;
      const numeric = ["integer", "float"];
      const comparison = left.type === right.type || (numeric.includes(left.type) && numeric.includes(right.type))
        ? x < y ? -1 : x > y ? 1 : 0 : left.type.localeCompare(right.type);
      return direction * comparison;
    });
    for (const item of ordered) {
      const row = document.createElement("tr");
      const idCell = document.createElement("td");
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = item.id;
      button.addEventListener("click", () => run(() => loadRecord(path(`/vectors/${item.id}`))));
      idCell.append(button);
      const metadata = document.createElement("td");
      metadata.textContent = JSON.stringify(item.metadata);
      row.append(idCell, metadata);
      for (const field of columns) {
        const cell = document.createElement("td");
        const value = item.metadata[field];
        cell.textContent = value ? `${value.value} (${value.type})` : "(missing)";
        row.append(cell);
      }
      table.append(row);
    }
  }
  for (const name of ["browse-columns", "browse-sort", "browse-direction"]) $(name).addEventListener("change", renderBrowse);
  $("browse-form").addEventListener("submit", (event) => {
    event.preventDefault();
    run(() => browse(null));
  });
  $("next-page").addEventListener("click", () => {
    if (nextAfter !== null) run(() => browse(nextAfter));
  });
  $("refresh-index").addEventListener("click", () => run(refreshIndex));
  // Prefill search/index controls from the selected collection's stored settings so the
  // explicit values the console sends match that collection (never another collection's).
  function applySettings(info) {
    const settings = info?.settings;
    if (!settings) return;
    $("metric").value = settings.metric;
    $("hnsw-m").value = String(settings.hnsw.m);
    $("hnsw-ef").value = String(settings.hnsw.ef_construction);
    $("hnsw-seed").value = String(settings.hnsw.seed);
    $("quantization").value = settings.quantization;
    $("rerank-multiplier").value = String(settings.rerank_multiplier);
    modeChanged();
  }
  function showCollectionStats(info) {
    dimensions = info.dimensions;
    // Refill only untouched example fields so user input is never overwritten.
    const example = exampleVector(info.dimensions);
    for (const id of ["record-vector", "query"]) if ($(id).value === exampleText) $(id).value = example;
    exampleText = example;
    $("stat-records").textContent = String(info.live_records ?? info.records);
    $("stat-dimensions").textContent = String(info.dimensions);
  }
  function populateCollections(items, selected) {
    collectionItems = items;
    $("collection").replaceChildren(...items.map((item) => new Option(`${item.name} (${item.dimensions} dims, ${item.live_records} records)`, item.name)));
    $("collection").value = selected;
  }
  // Pending (unsaved) collection-bound state that a collection switch would discard.
  function pendingWork() {
    const lost = [];
    if (editorDirty) lost.push("unsaved record editor changes");
    if (loaded) lost.push(`loaded record ${loaded.id}${loaded.revision ? ` (revision ${loaded.revision})` : ""}`);
    if (lastSearch) lost.push("search results");
    if ($("query").value.replace(/\s/g, "") !== exampleText.replace(/\s/g, "") || $("filter").value.replace(/\s/g, "") !== "[]" || $("search-text").value) lost.push("query vector, filter and text");
    return lost.concat(tools?.dirty() || []);
  }
  async function switchCollection(next) {
    if (next === collection) return;
    if (!collectionItems.some((item) => item.name === next)) throw new Error("Unknown collection; reconnect to refresh the list");
    const lost = pendingWork();
    if (lost.length && !await confirmOperation(`Switch from collection ${collection} to ${next}? This discards: ${lost.join("; ")}. Nothing is written to either collection; an open import job stays open on the server and can be resumed by job ID.`)) {
      status(`Still using collection ${collection}`);
      return;
    }
    const info = await api("GET", `/v1/collections/${next}`);
    if (info.name !== next) throw new Error("Server returned a different collection; selection unchanged");
    collection = next;
    resetCollectionData();
    tools?.collectionChanged(info);
    $("collection").value = next;
    collectionState();
    showCollectionStats(info);
    applySettings(info);
    $("snapshot-note").textContent = `Snapshot of collection ${next}. Refresh after external writes.`;
    if (can("read")) await refreshIndex(true);
    status(`Collection ${next} selected; previous results cleared`);
  }
  $("collection").addEventListener("change", () => {
    const next = $("collection").value;
    $("collection").value = collection; // revert until the switch is confirmed and verified
    run(() => switchCollection(next));
  });
  $("connection").addEventListener("submit", (event) => {
    event.preventDefault();
    run(async () => {
      if (!tools) throw new Error("Console tools failed to initialize; reload the page. Not connecting (fails closed).");
      token = $("token").value || token;
      $("token").value = "";
      const previous = collection;
      clearData();
      tools?.clear();
      connected = true;
      capabilities = null;
      const reported = await api("GET", "/v1/capabilities");
      if (reported.api_version !== 1 || !["admin", "read_only", "scoped"].includes(reported.role)) {
        disconnect();
        throw new Error("Unsupported server API version or role; use a matching console");
      }
      capabilities = reported;
      let info = null;
      if (extended()) {
        const listed = await api("GET", "/v1/collections");
        if (!listed.items.length) throw new Error("This credential covers no existing collection");
        const names = listed.items.map((item) => item.name);
        const selected = names.includes(previous) ? previous : names.includes("default") ? "default" : names[0];
        populateCollections(listed.items, selected);
        collection = selected;
        info = listed.items.find((item) => item.name === selected);
      }
      collectionState();
      const overview = {};
      if (capabilities.role !== "scoped") overview.database = await api("GET", "/v1/database");
      if (capabilities.role !== "scoped" || can("diagnostics")) overview.metrics = await api("GET", "/metrics");
      if (info) overview.collection = info;
      show("overview", overview);
      $("encryption-badge").textContent = overview.database?.is_encrypted === true
        ? `Encrypted (key ID ${overview.database.encryption_key_id ?? "unavailable"})`
        : overview.database?.is_encrypted === false ? "Encryption: off" : "Encryption: unavailable to this credential/server";
      showCollectionStats(collection === "default" && overview.database ? overview.database : info);
      $("stat-bytes").textContent = String(overview.metrics?.database_bytes ?? "Unavailable");
      $("snapshot-note").textContent = "Snapshot from the last connection refresh. External writes may change these values.";
      if (info) applySettings(info);
      if (can("read")) await refreshIndex(true);
      show("compatibility", capabilities);
      tools.connected(capabilities);
      $("mode").querySelector('option[value="approximate"]').disabled = !can("search_build");
      $("mode").querySelector('option[value="auto"]').disabled = !feature("search_auto");
      $("mode").querySelector('option[value="hybrid"]').disabled = !feature("search_hybrid");
      $("explain").disabled = !feature("search_explain");
      if ($("mode").selectedOptions[0]?.disabled) $("mode").value = "exact";
      modeChanged();
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
  // If-Match is sent only for the exact record (collection + ID) last loaded with an ETag.
  function preconditionFor(id) {
    if (!$("if-match").checked || !loaded || loaded.collection !== collection || loaded.id !== id || !loaded.etag) return null;
    return loaded;
  }
  function preconditionFailed(error) {
    if (error.status === 412) {
      loaded = null;
      $("record-revision").textContent = "Conflict: the record changed or was deleted since it was loaded. Nothing was written. Get it again to see the current revision, then reapply your edit.";
    }
  }
  $("vector-form").addEventListener("submit", (event) => {
    event.preventDefault();
    run(async () => {
      const target = recordPath();
      const owner = collection;
      const id = $("id").value;
      const metadata = parse("metadata");
      if (!metadata || Array.isArray(metadata) || typeof metadata !== "object") throw new Error("Metadata must be a typed JSON object");
      utils().metadata(metadata);
      const ttl = utils().expiry($("record-ttl").value.trim());
      if (ttl.ttl_ms !== undefined && !feature("record_expiry")) throw new Error("This server does not support record expiry");
      const body = { vector: vector("record-vector"), payload_base64: $("payload").value, metadata, ...ttl };
      const condition = preconditionFor(id);
      if (!await confirmOperation(`Replace ${target} (record ${id} in collection ${owner})? The vector, payload and entire metadata map will be durably replaced.${condition ? ` Only if unchanged since revision ${condition.revision ?? "loaded"}.` : " Unconditional write."}`)) return;
      if (owner !== collection) throw new Error("Collection changed before the write; nothing sent");
      try {
        await api("PUT", target, body, condition ? { headers: { "If-Match": condition.etag } } : {});
      } catch (error) {
        preconditionFailed(error);
        throw error;
      }
      loaded = null;
      editorDirty = false;
      $("record-revision").textContent = `Saved to collection ${owner}. Get the record again before another conditional edit.`;
      $("hits").querySelector("tbody").replaceChildren();
      show("record", { committed: true, collection: owner, id });
      clearBrowse();
      $("index-status").textContent = "Data changed; refresh index status";
      staleSnapshot();
    });
  });
  $("patch-metadata").addEventListener("click", () => run(async () => {
    const target = recordPath();
    const owner = collection;
    const condition = preconditionFor($("id").value);
    const metadata = utils().metadata(parse("metadata"));
    if (!feature("metadata_updates") || !can("write")) throw new Error("HTTP 403: metadata updates are unavailable for this credential/server");
    if (!await confirmOperation(`Replace only metadata for ${target} in collection ${owner}? Vector, payload, named vectors and expiry are retained.${condition ? " Only if unchanged since Get." : " Unconditional update."}`)) return;
    try {
      await api("PATCH", `${target}/metadata`, { metadata }, condition ? { headers: { "If-Match": condition.etag } } : {});
    } catch (error) { preconditionFailed(error); throw error; }
    loaded = null;
    // Only metadata was saved: vector, payload and TTL drafts may still be unsaved.
    editorDirty = true;
    mutationChanged();
    $("record-revision").textContent = "Metadata saved. Get again before another conditional edit. Other editor drafts were not written.";
    show("record", { metadata_updated: true, collection: owner });
  }));
  $("delete-vector").addEventListener("click", () => run(async () => {
    const target = recordPath();
    const owner = collection;
    const id = $("id").value;
    const condition = preconditionFor(id);
    if (!await confirmOperation(`Durably delete ${target} (record ${id} in collection ${owner})? This cannot be undone by the dashboard.${condition ? ` Only if unchanged since revision ${condition.revision ?? "loaded"}.` : ""}`)) return;
    if (owner !== collection) throw new Error("Collection changed before the delete; nothing sent");
    try {
      show("record", await api("DELETE", target, undefined, condition ? { headers: { "If-Match": condition.etag } } : {}));
    } catch (error) {
      preconditionFailed(error);
      throw error;
    }
    loaded = null;
    editorDirty = false;
    $("record-revision").textContent = "";
    $("hits").querySelector("tbody").replaceChildren();
    clearBrowse();
    $("index-status").textContent = "Data changed; refresh index status";
    staleSnapshot();
  }));
  function searchBody() {
    const mode = $("mode").value;
    const hybrid = mode === "hybrid";
    const body = { k: Number($("k").value), mode, metric: $("metric").value, config: indexConfig() };
    if (!Number.isInteger(body.k) || body.k < 0 || body.k > 10000) throw new Error("k must be 0..10000");
    const query = $("query").value.trim();
    if (!hybrid || query) body.vector = vector("query");
    body.filter = parse("filter");
    if (!Array.isArray(body.filter)) throw new Error("Filter must be a JSON array");
    if (mode === "auto" && !feature("search_auto")) throw new Error("This server does not advertise auto search");
    if (hybrid) {
      if (!feature("search_hybrid")) throw new Error("This server does not advertise hybrid search");
      const text = $("search-text").value.trim();
      if (!text && !body.vector) throw new Error("Hybrid search needs query text, a vector, or both");
      if (body.k > 1000) throw new Error("Hybrid k must be 0..1000");
      if (text) body.text = text;
      body.vector_leg = $("vector-leg").value;
      const rrf = utils().boundedInteger($("rrf-k").value, 1, 10000, "RRF k");
      const weight = (id, label) => {
        const value = Number($(id).value);
        if (!Number.isFinite(value) || value < 0 || value > 1000) throw new Error(`${label} must be 0..1000`);
        return value;
      };
      body.fusion = { rrf_k: rrf, vector_weight: weight("vector-weight", "Vector weight"), text_weight: weight("text-weight", "Text weight"), candidates: utils().boundedInteger($("fusion-candidates").value, 1, 10000, "Fusion candidates") };
    }
    const ann = ["approximate", "approximate_ready", "auto"].includes(mode) || (hybrid && body.vector_leg === "approximate_ready");
    if (ann) {
      if (mode !== "auto" && body.filter.length && !feature("filtered_ann")) throw new Error("This server does not advertise filtered ANN; use exact search or upgrade the server");
      body.ef_search = Number($("ef").value);
      if (!Number.isInteger(body.ef_search) || body.ef_search < 1 || body.ef_search > 100000) throw new Error("ef must be 1..100000");
      Object.assign(body, annOptions());
    }
    if ($("explain").checked && feature("search_explain")) body.explain = true;
    Object.assign(body, utils().rerank($("rerank-mode").value, $("rerank-candidates").value, $("mmr-lambda").value, body.k, mode));
    return body;
  }
  async function search() {
      const table = $("hits").querySelector("tbody");
      table.replaceChildren();
      $("search-summary").textContent = "Search not completed";
      $("search-plan").textContent = "";
      const owner = collection;
      const body = searchBody();
      const started = performance.now();
      const result = await api("POST", settingsPath("/search"), body);
      if (owner !== collection) throw new Error("Collection changed during search; results discarded");
      lastSearch = { body, result, collection: owner };
      const direction = utils().scoreDirection(body.mode, body.metric);
      $("search-summary").textContent = `${result.hits.length} neighbors returned in ${(performance.now() - started).toFixed(1)} ms (browser-observed); collection ${owner}; ${body.mode}, ${body.metric}. ${direction.label}. ${body.mode !== "exact" && body.filter.length ? "Filter-aware ANN may return fewer than k; no exact fallback. " : ""}Select an ID to inspect.`;
      $("hits").querySelector("caption").textContent = body.mode === "hybrid"
        ? `Search results (${body.metric}; hybrid score, higher is better)`
        : `Search results (${body.metric ?? "squared_l2"}; lower distance is better)`;
      const columns = body.mode === "hybrid" ? ["score", "distance", "bm25", "vector_rank", "text_rank"] : ["distance"];
      const header = $("hits").querySelector("thead tr");
      header.replaceChildren();
      for (const title of ["ID", ...columns]) {
        const cell = document.createElement("th"); cell.scope = "col";
        cell.textContent = title === "ID" ? "ID" : title === "score" ? "Score (higher is better)" : title === "distance" ? "Distance (lower is better)" : title;
        cell.dataset.field = title === "ID" ? "id" : title;
        header.append(cell);
      }
      for (const hit of result.hits) {
        const row = document.createElement("tr");
        const idCell = document.createElement("td");
        const button = document.createElement("button");
        button.type = "button";
        button.textContent = hit.id;
        button.addEventListener("click", () => run(() => inspectRecord(utils().collectionPath(owner, `/vectors/${hit.id}`))));
        idCell.append(button);
        row.append(idCell);
        for (const field of columns) {
          const cell = document.createElement("td");
          cell.textContent = hit[field] === null || hit[field] === undefined ? "--" : String(hit[field]);
          row.append(cell);
        }
        table.append(row);
      }
      if (result.plan || result.stats) show("search-plan", { plan: result.plan ?? null, stats: result.stats ?? null });
  }
  $("search-form").addEventListener("submit", (event) => {
    event.preventDefault();
    run(search);
  });
  $("build-index").addEventListener("click", () => run(async () => {
    const owner = collection;
    if (!await confirmOperation(`Build HNSW for collection ${owner} now? This synchronously blocks the single database owner and clears that collection's mutation buffer.`)) return;
    $("build-result").textContent = "Building; no automatic retry";
    const started = performance.now();
    try {
      if ($("metric").value === "inner_product") throw new Error("HNSW supports squared L2 and cosine, not inner product");
      if (owner !== collection) throw new Error("Collection changed before the build; nothing sent");
      await api("POST", settingsPath("/index/hnsw"), { metric: $("metric").value, config: indexConfig(), ...annOptions() });
      $("build-result").textContent = `Build completed in ${(performance.now() - started).toFixed(1)} ms (browser-observed)`;
      await refreshIndex();
    } catch (error) {
      $("build-result").textContent = "Build failed or outcome unknown; inspect the response";
      throw error;
    }
  }));
  try {
  tools = window.VLiteTools.init({
    api, run, status, confirm: confirmOperation, inspect: inspectRecord, search, indexConfig, disconnect,
    dimensions: () => dimensions, busy: () => busy, connected: () => connected,
    capabilities: () => capabilities, lastSearch: () => lastSearch,
    collection: () => collection, path, settingsPath, feature, can, extended, annOptions, searchBody,
    collections: () => collectionItems,
    // Called by collection management after create/drop/settings so the selector stays truthful.
    refreshCollections: async () => {
      const listed = await api("GET", "/v1/collections");
      const current = listed.items.find((item) => item.name === collection);
      if (!current) {
        populateCollections(listed.items, listed.items[0]?.name);
        collection = listed.items[0]?.name ?? "default";
        resetCollectionData();
        tools?.collectionChanged(listed.items[0] ?? null);
        collectionState();
        if (listed.items[0]) { showCollectionStats(listed.items[0]); applySettings(listed.items[0]); }
      } else {
        populateCollections(listed.items, collection);
        showCollectionStats(current);
      }
      return listed;
    },
    applySettings: (info) => { if (info.name === collection) applySettings(info); },
  });
  } catch (error) {
    // Fail closed with an explicit message instead of a later null dereference.
    tools = null;
    status(`Console tools failed to initialize (${error.message}); reload the page. Connecting is disabled.`, true);
  }
  // A read-only bridge shares the in-memory connection without exposing its token.
  window.VLiteObservabilityConnection = {
    connected: () => connected,
    request: async (route, options = {}) => {
      if (!connected) throw new Error("Connect before accessing the database");
      const endpoint = route.split("?")[0];
      if (!["/metrics", "/v1/admin/metrics/summary", "/v1/admin/logs", "/v1/admin/diagnostics/slow-queries", "/v1/admin/audit"].includes(endpoint)) throw new Error("Unsupported observability endpoint");
      // Background reads must not replace the foreground request's cancel handle or inspector.
      const controller = new AbortController();
      const timeout = setTimeout(() => controller.abort(), 15000);
      try {
        const response = await fetch(`${apiBase}${route}`, {
          method: "GET", signal: controller.signal, cache: "no-store", credentials: "omit", redirect: "error",
          headers: { Authorization: `Bearer ${token}` },
        });
        if (!response.ok) {
          const error = new Error(`HTTP ${response.status}`);
          error.status = response.status;
          throw error;
        }
        return options.text ? await response.text() : await response.json();
      } finally { clearTimeout(timeout); }
    },
  };
  window.addEventListener("pagehide", disconnect);
})();
