"use strict";
(() => {
  const LEVELS = ["trace", "debug", "info", "warn", "error"];
  const finite = (value) => value !== null && value !== undefined && value !== "" && Number.isFinite(Number(value)) ? Number(value) : null;
  const list = (value) => Array.isArray(value) ? value : [];
  function parsePrometheus(text) {
    const samples = [];
    for (const line of String(text).split(/\r?\n/)) {
      if (!line.trim() || /^\s*#/.test(line)) continue;
      const match = line.match(/^\s*([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{((?:[^"\\]|\\.|"(?:[^"\\]|\\.)*")*)\})?\s+([+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?|[+-]?Inf|NaN)(?:\s+\S+)?\s*$/);
      if (!match) continue;
      const labels = Object.create(null);
      let rest = (match[2] || "").trim(), valid = true;
      while (rest) {
        const label = rest.match(/^([a-zA-Z_][a-zA-Z0-9_]*)\s*=\s*"((?:[^"\\]|\\[\\"n])*)"\s*(,|$)/);
        if (!label || Object.hasOwn(labels, label[1])) { valid = false; break; }
        labels[label[1]] = label[2].replace(/\\([\\"n])/g, (_, escaped) => escaped === "n" ? "\n" : escaped);
        rest = rest.slice(label[0].length).trim();
      }
      const value = match[3] === "+Inf" || match[3] === "Inf" ? Infinity : match[3] === "-Inf" ? -Infinity : Number(match[3]);
      if (valid) samples.push({ name: match[1], labels, value });
    }
    return samples;
  }
  function counterRate(current, previous, elapsedSeconds) {
    const a = finite(current), b = finite(previous);
    return a === null || b === null || !(elapsedSeconds > 0) || a < b ? null : (a - b) / elapsedSeconds;
  }
  function deriveRates(current, previous) {
    const elapsed = previous ? (current.ts_ms - previous.ts_ms) / 1000 : 0;
    const requests = counterRate(current.requests?.total, previous?.requests?.total, elapsed);
    const errors = counterRate(current.requests?.errors, previous?.requests?.errors, elapsed);
    const searches = counterRate(current.search?.total, previous?.search?.total, elapsed);
    return { requests, errors, errorPercent: requests > 0 && errors !== null ? errors / requests * 100 : requests === 0 && errors === 0 ? 0 : null, searches };
  }
  function histogramQuantile(buckets, quantile) {
    const sorted = buckets.filter((b) => !Number.isNaN(b.bound) && Number.isFinite(b.count) && b.count >= 0).sort((a, b) => a.bound - b.bound);
    const total = sorted.at(-1)?.count;
    if (!(total > 0)) return null;
    const rank = total * quantile;
    let lower = 0, count = 0;
    for (const bucket of sorted) {
      if (bucket.count < count) return null;
      if (bucket.count >= rank) {
        if (!Number.isFinite(bucket.bound)) return lower;
        return bucket.count === count ? bucket.bound : lower + (bucket.bound - lower) * (rank - count) / (bucket.count - count);
      }
      lower = bucket.bound; count = bucket.count;
    }
    return null;
  }
  function prometheusSummary(samples, now = Date.now()) {
    const sum = (names) => {
      for (const name of names) {
        const found = samples.filter((s) => s.name === name && Number.isFinite(s.value));
        if (found.length) {
          const unlabelled = found.find((s) => !Object.keys(s.labels).length);
          return unlabelled ? unlabelled.value : found.reduce((n, s) => n + s.value, 0);
        }
      }
      return null;
    };
    const latency = (kind, q, method, route) => {
      for (const name of [`vlite_${kind}_duration_seconds`, `vlite_${kind}_latency_seconds`]) {
        const buckets = new Map();
        for (const s of samples.filter((s) => s.name === `${name}_bucket` && (route === undefined || (s.labels.route || s.labels.path) === route && (s.labels.method || "") === method))) {
          const bound = s.labels.le === "+Inf" ? Infinity : Number(s.labels.le);
          buckets.set(bound, (buckets.get(bound) || 0) + s.value);
        }
        const result = histogramQuantile(Array.from(buckets, ([bound, count]) => ({ bound, count })), q);
        if (result !== null) return result * 1000;
      }
      return null;
    };
    const routes = new Map();
    for (const s of samples) {
      const route = s.labels.route || s.labels.path;
      if (!route || !Number.isFinite(s.value)) continue;
      const key = `${s.labels.method || ""} ${route}`;
      if (!routes.has(key)) routes.set(key, { method: s.labels.method || "", route, count: null, errors: null, p95_ms: null });
      const item = routes.get(key);
      if (s.name === "vlite_requests_total") {
        item.count = (item.count || 0) + s.value;
        if (Number(s.labels.status) >= 400) item.errors = (item.errors || 0) + s.value;
      }
      if (["vlite_request_failures_total", "vlite_request_errors_total"].includes(s.name)) item.errors = (item.errors || 0) + s.value;
      if (s.name === "vlite_request_duration_seconds" && s.labels.quantile === "0.95") item.p95_ms = s.value * 1000;
    }
    for (const row of routes.values()) {
      row.p50_ms = latency("request", .5, row.method, row.route);
      row.p95_ms = row.p95_ms ?? latency("request", .95, row.method, row.route);
      row.p99_ms = latency("request", .99, row.method, row.route);
    }
    // Older servers expose only cumulative duration, not latency percentiles.
    const searchCount = sum(["vlite_searches_total", "vlite_search_requests_total", "vlite_search_duration_seconds_count"]);
    const searchSum = sum(["vlite_search_duration_seconds_sum", "vlite_search_duration_seconds_total"]);
    return { ts_ms: now, requests: { total: sum(["vlite_requests_total", "vlite_request_duration_seconds_count"]), errors: sum(["vlite_request_failures_total", "vlite_request_errors_total"]), in_flight: sum(["vlite_requests_in_flight", "vlite_in_flight"]), p50_ms: latency("request", .5), p95_ms: latency("request", .95), by_route: [...routes.values()] },
      search: { total: searchCount, p95_ms: latency("search", .95), mean_ms: searchCount > 0 && searchSum !== null ? searchSum / searchCount * 1000 : null },
      database: { vectors: sum(["vlite_live_records", "vlite_vectors_total", "vlite_collection_live_records"]), collections: sum(["vlite_collections_total"]), file_bytes: sum(["vlite_database_bytes"]) } };
  }
  function filterLogs(entries, { level = "trace", q = "", target = "" } = {}) {
    const minimum = Math.max(0, LEVELS.indexOf(level.toLowerCase()));
    const query = q.trim().toLowerCase();
    return list(entries).filter((entry) => {
      const severity = String(entry.level || "info").toLowerCase().replace("warning", "warn");
      return LEVELS.indexOf(severity) >= minimum && (!target || String(entry.target || "") === target) &&
        (!query || `${entry.message || ""} ${entry.target || ""} ${JSON.stringify(entry.fields || {})}`.toLowerCase().includes(query));
    });
  }
  function formatLog(entry) {
    const timestamp = finite(entry.ts_ms ?? entry.unix_ms ?? entry.timestamp_unix_ms ?? entry.timestamp_ms);
    return { time: timestamp !== null && !Number.isNaN(new Date(timestamp).getTime()) ? new Date(timestamp).toISOString() : "Unknown time",
      level: String(entry.level || "info").toLowerCase().replace("warning", "warn"), target: String(entry.target || "server"), message: String(entry.message || ""), fields: JSON.stringify(entry.fields ?? {}, null, 2) };
  }
  function mergeLogs(current, incoming, capacity = 5000) {
    const seen = new Set(current.map((entry) => String(entry.seq)));
    return [...current, ...list(incoming).filter((entry) => {
      const key = String(entry.seq);
      if (seen.has(key)) return false;
      seen.add(key); return true;
    })].slice(-capacity);
  }
  const helpers = { parsePrometheus, counterRate, deriveRates, histogramQuantile, prometheusSummary, filterLogs, formatLog, mergeLogs };
  if (typeof module !== "undefined" && module.exports) module.exports = helpers;
  if (typeof document === "undefined") return;
  const $ = (id) => document.getElementById(id);
  const section = $("observability");
  if (!section) return;
  let previous = null, source = null, entries = [], since = 0, target = "", paused = false, tailPaused = false;
  let inView = false, loading = false, generation = 0, timer = null, dropped = 0;
  let sortKey = "count", sortAscending = false, routeRows = [], endpointSupported = true;
  const history = new Map();
  const connection = () => window.VLiteObservabilityConnection;
  const connected = () => connection()?.connected();
  const message = (id, value) => { $(id).textContent = value; };
  const number = (value, digits = 1) => finite(value) === null ? "—" : Number(value).toLocaleString(undefined, { maximumFractionDigits: digits });
  const failure = (error) => error.status === 401 || error.status === 403 ? "Super admin needed. Sign in as a super admin or with the admin token to view this data." : error.status === 404 ? "Endpoint not available on this server version." : error.message?.includes("Sign in before") ? "Sign in first." : "Network or server error. Check the connection and refresh to retry.";
  async function get(path, text = false) { return connection().request(path, { text }); }
  const make = (tag, text, className) => {
    const element = document.createElement(tag);
    if (text !== undefined) element.textContent = text;
    if (className) element.className = className;
    return element;
  };
  function drawSparkline(id, value) {
    if (finite(value) === null) return;
    const points = [...(history.get(id) || []), Number(value)].slice(-40);
    history.set(id, points);
    const svg = $(`obs-spark-${id}`), low = Math.min(...points), range = Math.max(...points) - low || 1;
    const polyline = document.createElementNS("http://www.w3.org/2000/svg", "polyline");
    polyline.setAttribute("points", points.map((v, i) => `${i * 120 / Math.max(1, points.length - 1)},${30 - (v - low) / range * 26}`).join(" "));
    svg.replaceChildren(polyline);
  }
  function card(id, value, suffix = "", digits = 1) {
    message(`obs-${id}`, finite(value) === null ? "—" : `${number(value, digits)}${suffix}`);
    drawSparkline(id, value);
  }
  function renderRoutes() {
    const rows = [...routeRows].sort((a, b) => ((finite(a[sortKey]) ?? -1) - (finite(b[sortKey]) ?? -1)) * (sortAscending ? 1 : -1));
    const body = $("obs-routes"); body.replaceChildren();
    for (const route of rows) {
      const row = make("tr");
      for (const value of [`${route.method || ""} ${route.route || "unknown"}`, number(route.count ?? route.total, 0), number(route.errors, 0), number(route.p50_ms), number(route.p95_ms), number(route.p99_ms)]) row.append(make("td", value));
      body.append(row);
    }
    for (const th of section.querySelectorAll("th[data-obs-sort]")) th.setAttribute("aria-sort", th.dataset.obsSort === sortKey ? sortAscending ? "ascending" : "descending" : "none");
  }
  function renderMetrics(data, rates) {
    const req = data.requests || {}, search = data.search || {}, db = data.database || {};
    card("rps", rates.requests, " /s"); card("errors", rates.errorPercent, "%");
    card("p50", req.p50_ms ?? req.latency?.p50_ms, " ms"); card("p95", req.p95_ms ?? req.latency?.p95_ms, " ms");
    card("search-qps", rates.searches, " /s"); card("search-latency", search.p95_ms ?? search.latency?.p95_ms ?? search.mean_ms, " ms");
    card("vectors", db.vectors, "", 0); card("collections", db.collections, "", 0); card("in-flight", req.in_flight, "", 0);
    routeRows = list(req.by_route); renderRoutes();
    message("obs-metrics-status", `${source === "prometheus" ? "Prometheus fallback · histogram percentiles are lifetime estimates" : "Live metrics"} · ${new Date().toLocaleTimeString()} · Uptime ${number(data.uptime_s, 0)}s · File ${number(db.file_bytes, 0)} bytes${db.read_only ? " · Read-only" : ""}. ${rates.requests === null ? "Rates need two samples; — means unavailable." : "— means unavailable."}`);
  }
  function renderLogs(newCount = 0) {
    const filtered = filterLogs(entries, { level: $("obs-level").value, q: $("obs-log-search").value, target });
    const viewport = $("obs-log-view");
    const scrollTop = viewport.scrollTop;
    const expanded = new Set(Array.from(viewport.querySelectorAll("details[open]"), (row) => row.dataset.seq));
    const fragment = document.createDocumentFragment();
    for (const entry of filtered.slice(-500)) {
      const log = formatLog(entry), details = make("details", undefined, "obs-log-row"), summary = make("summary");
      summary.append(make("time", log.time), make("span", log.level, `obs-level obs-level-${LEVELS.includes(log.level) ? log.level : "info"}`), make("span", log.target, "obs-target"), make("span", log.message, "obs-message"));
      details.dataset.seq = String(entry.seq); details.open = expanded.has(String(entry.seq));
      details.append(summary, make("pre", log.fields)); fragment.append(details);
    }
    viewport.replaceChildren(fragment);
    if (!filtered.length) viewport.append(make("p", "No matching log entries. Try a different level, text, or target."));
    message("obs-log-count", `${newCount ? `${newCount} new entries. ` : ""}${filtered.length} matching / ${entries.length} buffered (maximum 5,000; newest 500 displayed). Dropped on server: ${number(dropped, 0)}.`);
    const chips = $("obs-targets"); chips.replaceChildren();
    for (const name of ["", ...new Set(entries.map((entry) => String(entry.target || "server")))].slice(0, 40)) {
      const chip = make("button", name || "All targets", "secondary"); chip.type = "button"; chip.setAttribute("aria-pressed", String(target === name));
      chip.addEventListener("click", () => { target = name; renderLogs(); }); chips.append(chip);
    }
    viewport.scrollTop = $("obs-auto-scroll").checked ? viewport.scrollHeight : scrollTop;
  }
  function renderRecords(id, values, columns) {
    const body = $(id); body.replaceChildren();
    for (const value of list(values).slice(-100).reverse()) {
      const row = make("tr");
      for (const column of columns) row.append(make("td", column(value)));
      const cell = make("td"), details = make("details"), summary = make("summary", "Details");
      details.append(summary, make("pre", JSON.stringify(value, null, 2))); cell.append(details); row.append(cell); body.append(row);
    }
  }
  async function metrics(epoch) {
    try {
      let data, nextSource = source;
      if (source !== "prometheus") {
        try { data = await get("/v1/admin/metrics/summary"); nextSource = "summary"; }
        catch (error) { if (error.status !== 404) throw error; nextSource = "prometheus"; }
      }
      if (nextSource === "prometheus") data = prometheusSummary(parsePrometheus(await get("/metrics", true)));
      if (epoch !== generation) return;
      source = nextSource;
      data = data && typeof data === "object" ? { ...data } : {};
      data.ts_ms = finite(data.ts_ms) ?? Date.now();
      const rates = deriveRates(data, previous); previous = data;
      renderMetrics(data, rates);
    } catch (error) { if (epoch === generation) message("obs-metrics-status", failure(error)); }
  }
  async function logs(epoch) {
    if (tailPaused || !endpointSupported) return;
    try {
      const query = new URLSearchParams({ since: String(since), level: $("obs-level").value, q: $("obs-log-search").value, limit: "1000" });
      const data = await get(`/v1/admin/logs?${query}`);
      if (epoch !== generation) return;
      if (tailPaused) return;
      const before = new Set(entries.map((entry) => String(entry.seq)));
      const incoming = list(data?.entries);
      entries = mergeLogs(entries, incoming);
      // Keep sequence strings intact if the server uses full-width u64 cursors.
      since = data?.next_seq ?? incoming.at(-1)?.seq ?? since;
      dropped = data?.dropped ?? 0;
      renderLogs(incoming.filter((entry) => !before.has(String(entry.seq))).length);
      message("obs-logs-status", `Tailing server log ring · capacity ${number(data?.capacity, 0)}. Entries are memory-only and expire on restart.`);
    } catch (error) {
      if (epoch !== generation) return;
      if ([401, 403, 404].includes(error.status)) endpointSupported = false;
      message("obs-logs-status", failure(error));
    }
  }
  async function diagnostics(epoch, path, statusId, bodyId, columns) {
    try {
      const data = await get(path);
      if (epoch !== generation) return;
      renderRecords(bodyId, data?.items ?? data?.entries, columns);
      message(statusId, `Most recent entries · ${new Date().toLocaleTimeString()}${data?.threshold_ms !== undefined ? ` · slow threshold ${number(data.threshold_ms)} ms` : ""}`);
    } catch (error) { if (epoch === generation) message(statusId, failure(error)); }
  }
  const timeOf = (entry) => formatLog(entry).time;
  const isActive = () => inView && !document.hidden && !section.hidden && section.getClientRects().length > 0 && connected();
  function schedule() {
    clearTimeout(timer);
    const interval = Number($("obs-interval").value);
    if (!paused && interval && isActive()) timer = setTimeout(() => refresh(), interval);
  }
  async function refresh(manual = false) {
    if (loading || !isActive() || (paused && !manual)) { schedule(); return; }
    loading = true;
    const epoch = generation;
    await Promise.all([
      metrics(epoch), logs(epoch),
      diagnostics(epoch, "/v1/admin/diagnostics/slow-queries", "obs-slow-status", "obs-slow-rows", [timeOf, (e) => e.route || "search", (e) => e.collection || "default", (e) => number(e.duration_ms ?? e.elapsed_ms)]),
      diagnostics(epoch, "/v1/admin/audit", "obs-audit-status", "obs-audit-rows", [timeOf, (e) => String(e.event || e.action || "event"), (e) => String(e.route || e.collection || e.name || "—")]),
    ]);
    loading = false; schedule();
  }
  function reset() {
    generation++; previous = null; source = null; entries = []; since = 0; target = ""; dropped = 0; routeRows = []; endpointSupported = true; history.clear();
    for (const value of section.querySelectorAll(".obs-card-value")) value.textContent = "—";
    for (const svg of section.querySelectorAll(".obs-spark")) svg.replaceChildren();
    for (const id of ["obs-routes", "obs-log-view", "obs-targets", "obs-slow-rows", "obs-audit-rows"]) $(id).replaceChildren();
    for (const id of ["obs-metrics-status", "obs-logs-status", "obs-slow-status", "obs-audit-status"]) message(id, "Sign in as a super admin to view server observability.");
    message("obs-log-count", "No log entries buffered.");
    schedule();
  }
  $("obs-pause").addEventListener("click", () => {
    paused = !paused; $("obs-pause").textContent = paused ? "Resume monitoring" : "Pause monitoring"; $("obs-pause").setAttribute("aria-pressed", String(paused));
    if (!paused) refresh(true); else clearTimeout(timer);
  });
  $("obs-tail-pause").addEventListener("click", () => {
    tailPaused = !tailPaused; $("obs-tail-pause").textContent = tailPaused ? "Resume tail" : "Pause tail"; $("obs-tail-pause").setAttribute("aria-pressed", String(tailPaused));
    if (!tailPaused) refresh(true);
  });
  $("obs-refresh").addEventListener("click", () => { endpointSupported = true; refresh(true); });
  $("obs-interval").addEventListener("change", () => { schedule(); if (Number($("obs-interval").value)) refresh(); });
  for (const id of ["obs-level", "obs-log-search"]) $(id).addEventListener("input", () => {
    // Start a new bounded tail on filter change so previously excluded entries can be fetched.
    generation++; entries = []; since = 0; endpointSupported = true; renderLogs(); schedule();
  });
  $("obs-clear").addEventListener("click", () => { entries = []; renderLogs(); });
  $("obs-download").addEventListener("click", () => {
    const filtered = filterLogs(entries, { level: $("obs-level").value, q: $("obs-log-search").value, target });
    const url = URL.createObjectURL(new Blob([filtered.map((entry) => JSON.stringify(entry)).join("\n") + (filtered.length ? "\n" : "")], { type: "application/x-ndjson" }));
    const anchor = make("a"); anchor.href = url; anchor.download = "vlite-logs.jsonl"; document.body.append(anchor); anchor.click(); anchor.remove(); setTimeout(() => URL.revokeObjectURL(url), 1000);
  });
  for (const th of section.querySelectorAll("th[data-obs-sort]")) th.querySelector("button").addEventListener("click", () => {
    const key = th.dataset.obsSort; sortAscending = key === sortKey ? !sortAscending : false; sortKey = key; renderRoutes();
  });
  document.addEventListener("visibilitychange", () => { if (!document.hidden && Number($("obs-interval").value)) refresh(); else clearTimeout(timer); });
  if (typeof IntersectionObserver !== "undefined") new IntersectionObserver((changes) => {
    inView = changes[0].isIntersecting;
    if (inView && Number($("obs-interval").value)) refresh(); else clearTimeout(timer);
  }).observe(section);
  else { inView = true; schedule(); }
  const badge = $("connection-badge");
  if (badge) new MutationObserver(() => { reset(); if (connected() && Number($("obs-interval").value)) refresh(); }).observe(badge, { attributes: true, attributeFilter: ["data-connected"] });
  $("connection")?.addEventListener("submit", reset, { capture: true });
  window.addEventListener("pagehide", () => { generation++; clearTimeout(timer); });
  reset();
})();
