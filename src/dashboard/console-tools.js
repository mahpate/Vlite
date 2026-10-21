"use strict";
(() => {
  const $ = (id) => document.getElementById(id);
  const text = (id, value) => { $(id).textContent = typeof value === "string" ? value : JSON.stringify(value, null, 2); };
  const object = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
  function id(value) {
    if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/.test(value) || value.length > 20 || BigInt(value) > 18446744073709551615n) throw new Error("Expected a canonical decimal u64 string ID");
    return value;
  }
  function vector(value, dimensions) {
    if (!Array.isArray(value) || !value.length || !value.every((v) => typeof v === "number" && Number.isFinite(v) && Math.abs(v) <= 3.4028234663852886e38)) throw new Error("Expected a nonempty finite f32 vector");
    if (dimensions !== null && dimensions !== undefined && value.length !== dimensions) throw new Error(`Dimension mismatch: expected ${dimensions}, received ${value.length}`);
    return value;
  }
  function metadata(value) {
    if (!object(value) || Object.keys(value).length > 64) throw new Error("Metadata must be a typed object with at most 64 fields");
    let bytes = 4;
    for (const [field, entry] of Object.entries(value)) {
      if (!field || new TextEncoder().encode(field).length > 256 || !object(entry) || Object.keys(entry).sort().join(",") !== "type,value") throw new Error("Invalid metadata field or typed value");
      const { type, value: item } = entry;
      const valid = type === "string" ? typeof item === "string" && new TextEncoder().encode(item).length <= 4096
        : type === "boolean" ? typeof item === "boolean"
          : type === "float" ? typeof item === "number" && Number.isFinite(item)
            : type === "integer" && typeof item === "string" && /^(-?[1-9][0-9]*|0)$/.test(item) && item.length <= 20 && BigInt(item) >= -9223372036854775808n && BigInt(item) <= 9223372036854775807n;
      if (!valid) throw new Error(`Invalid ${type} metadata value for ${field}; integers use decimal strings`);
      bytes += 3 + new TextEncoder().encode(field).length + (type === "string" ? 4 + new TextEncoder().encode(item).length : type === "boolean" ? 1 : 8);
    }
    if (bytes > 65536) throw new Error("Encoded metadata exceeds 65536 bytes");
    return value;
  }
  function record(value, dimensions) {
    if (!object(value) || Object.keys(value).some((key) => !["id", "vector", "metadata", "payload_base64"].includes(key))) throw new Error("Unknown record fields");
    id(value.id);
    vector(value.vector, dimensions);
    metadata(value.metadata ?? {});
    const payload = value.payload_base64 ?? "";
    if (typeof payload !== "string" || !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(payload)) throw new Error("Invalid standard base64 payload");
    if (btoa(atob(payload)) !== payload) throw new Error("Payload base64 must use canonical padding bits");
    const payloadBytes = payload.length / 4 * 3 - (payload.endsWith("==") ? 2 : payload.endsWith("=") ? 1 : 0);
    if (payloadBytes > 1048576) throw new Error("Payload exceeds 1 MiB");
    return { id: value.id, vector: value.vector, metadata: value.metadata ?? {}, payload_base64: payload };
  }
  function csvParse(source) {
    if (source.startsWith("\uFEFF")) source = source.slice(1);
    const rows = [];
    let row = [], field = "", quoted = false, closed = false;
    for (let i = 0; i < source.length; i++) {
      const char = source[i];
      if (quoted) {
        if (char === '"') {
          if (source[i + 1] === '"') { field += '"'; i++; } else { quoted = false; closed = true; }
        } else field += char;
      } else if (char === '"' && !field && !closed) quoted = true;
      else if (char === ",") { row.push(field); field = ""; closed = false; }
      else if (char === "\n" || char === "\r") {
        if (char === "\r" && source[i + 1] === "\n") i++;
        row.push(field); rows.push(row); row = []; field = ""; closed = false;
      } else {
        if (closed || char === '"') throw new Error("Malformed CSV quoting");
        field += char;
      }
    }
    if (quoted) throw new Error("Unclosed CSV quote");
    if (field || row.length || closed) { row.push(field); rows.push(row); }
    if (!rows.length) throw new Error("CSV is empty");
    const headers = rows.shift();
    if (new Set(headers).size !== headers.length || !headers.includes("id") || !headers.includes("vector") || headers.some((h) => !["id", "vector", "metadata", "payload_base64"].includes(h))) throw new Error("CSV headers must include id,vector; optional metadata,payload_base64");
    return rows.map((cells) => {
      if (cells.length !== headers.length) throw new Error("CSV row has wrong number of columns");
      const item = Object.fromEntries(headers.map((key, index) => [key, cells[index]]));
      item.vector = JSON.parse(item.vector);
      item.metadata = item.metadata ? JSON.parse(item.metadata) : {};
      return item;
    });
  }
  const csvCell = (value) => {
    const safe = /^[=+\-@\t\r]/.test(value) ? "'" + value : value;
    return '"' + safe.replaceAll('"', '""') + '"';
  };
  function csvExport(items) {
    return "id,vector,payload_base64,metadata\r\n" + items.map((item) => [item.id, JSON.stringify(item.vector), item.payload_base64, JSON.stringify(item.metadata)].map(csvCell).join(",")).join("\r\n");
  }
  function distances(a, b) {
    vector(a, b.length); vector(b, a.length);
    let l2 = 0, dot = 0, aa = 0, bb = 0;
    for (let i = 0; i < a.length; i++) { l2 += (a[i] - b[i]) ** 2; dot += a[i] * b[i]; aa += a[i] ** 2; bb += b[i] ** 2; }
    return { squared_l2: l2, cosine: aa && bb ? 1 - dot / Math.sqrt(aa * bb) : null, inner_product: -dot };
  }
  function analyze(items) {
    const fields = new Map(), seen = new Map(), duplicates = [], zeros = [];
    for (const item of items) {
      if (item.vector.every((v) => v === 0)) zeros.push(item.id);
      const key = JSON.stringify(item.vector);
      if (seen.has(key)) duplicates.push([seen.get(key), item.id]); else seen.set(key, item.id);
      for (const [name, entry] of Object.entries(item.metadata)) {
        if (!fields.has(name)) fields.set(name, { types: new Set(), values: new Map() });
        const field = fields.get(name);
        field.types.add(entry.type);
        const value = JSON.stringify(entry);
        field.values.set(value, (field.values.get(value) || 0) + 1);
      }
    }
    return { sampled_records: items.length, zero_vector_ids: zeros, duplicate_vector_pairs: duplicates,
      fields: Array.from(fields, ([name, field]) => ({ name, types: [...field.types], mixed_types: field.types.size > 1, values: [...field.values].map(([value, count]) => ({ value: JSON.parse(value), count })) })) };
  }
  function pca(items) {
    if (items.length < 2) throw new Error("PCA requires at least two records");
    const dimensions = items[0].vector.length;
    if (dimensions > 256) throw new Error("PCA visualization is limited to 256 dimensions");
    const mean = Array(dimensions).fill(0);
    items.forEach((item) => item.vector.forEach((v, j) => { mean[j] += v / items.length; }));
    const data = items.map((item) => item.vector.map((v, j) => v - mean[j]));
    const dot = (a, b) => a.reduce((sum, v, j) => sum + v * b[j], 0);
    const components = [];
    for (let axis = 0; axis < 2; axis++) {
      let v = Array.from({ length: dimensions }, (_, j) => Math.sin((j + 1) * (axis + 1.31)));
      for (let step = 0; step < 40; step++) {
        const next = Array(dimensions).fill(0);
        data.forEach((row) => { const weight = dot(row, v); row.forEach((x, j) => { next[j] += x * weight; }); });
        for (const previous of components) { const weight = dot(next, previous); previous.forEach((x, j) => { next[j] -= weight * x; }); }
        const norm = Math.hypot(...next);
        v = norm > 1e-30 ? next.map((x) => x / norm) : Array(dimensions).fill(0);
      }
      components.push(v);
    }
    return data.map((row, index) => ({ id: items[index].id, x: dot(row, components[0]), y: dot(row, components[1]) }));
  }
  const readFile = async (file) => {
    if (!file || file.size > 4 * 1024 * 1024) throw new Error("Select a file no larger than 4 MiB");
    return (await file.text()).replace(/^\uFEFF/, "");
  };
  function download(name, data, type = "application/json") {
    const url = URL.createObjectURL(new Blob([typeof data === "string" ? data : JSON.stringify(data, null, 2)], { type }));
    const link = document.createElement("a");
    link.href = url; link.download = name; link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  }
  function init(ctx) {
    let preview = [], sample = [], refreshing = false;
    const on = (name, action) => $(name).addEventListener("click", () => ctx.run(action));
    const get = (name) => JSON.parse($(name).value);
    const fetchRecord = (value) => ctx.api("GET", `/v1/vectors/${id(value)}`);
    async function records(ids) {
      const result = [];
      let bytes = 0;
      for (const value of ids) {
        const item = await fetchRecord(value);
        bytes += new TextEncoder().encode(JSON.stringify(item)).length;
        if (bytes > 4 * 1024 * 1024) throw new Error("Full-record operation exceeds the 4 MiB browser working-set limit; reduce the page/selection size");
        result.push(item);
      }
      return result;
    }
    function pageIds() {
      const ids = [...$("vectors").querySelectorAll("tbody button")].map((button) => id(button.textContent));
      if (!ids.length) throw new Error("Load a browse page first");
      return ids;
    }
    function selectedIds() {
      const ids = $("batch-ids").value.split(",").map((v) => id(v.trim()));
      if (ids.length > 100 || new Set(ids).size !== ids.length || ids.some((v) => !pageIds().includes(v))) throw new Error("Select up to 100 unique IDs from the currently loaded page");
      return ids;
    }
    async function commit(items) {
      for (let start = 0; start < items.length; start += 20) {
        text("import-result", `${start}/${items.length} operations acknowledged; sending next atomic batch. Do not retry an unknown outcome.`);
        await ctx.api("POST", "/v1/batches", { operations: items.slice(start, start + 20) });
      }
      text("import-result", `${items.length}/${items.length} operations committed. Refresh the browse page.`);
    }
    function validatePreview(items) {
      if (ctx.dimensions() === null) throw new Error("Connect before validating records");
      if (!Array.isArray(items) || !items.length || items.length > 100) throw new Error("Import requires 1..100 records");
      const result = items.map((item) => record(item, ctx.dimensions()));
      if (new Set(result.map((item) => item.id)).size !== result.length) throw new Error("Duplicate IDs in import");
      if (new TextEncoder().encode(JSON.stringify(result)).length > 4 * 1024 * 1024) throw new Error("Import exceeds 4 MiB");
      return result;
    }
    on("demo-preview", async () => {
      preview = [];
      text("import-result", "Preparing demo preview");
      const size = ctx.dimensions();
      if (!size || size > 4096) throw new Error("Connect to a database with 1..4096 dimensions for the demo");
      preview = validatePreview([1, 2, 3].map((n) => ({ id: String(n), vector: Array.from({ length: size }, (_, j) => j === 0 ? n / 3 : j === 1 ? 1 - n / 3 : 0), metadata: { category: { type: "string", value: "demo" } } })));
      text("import-result", { instructions: "Preview only. Commit replaces IDs 1,2,3 after confirmation; then browse and Find similar to saved ID.", records: preview });
    });
    on("import-preview", async () => {
      preview = [];
      text("import-result", "Validating import; no writes have been sent");
      const file = $("import-file").files[0];
      const source = await readFile(file);
      preview = validatePreview(file.name.toLowerCase().endsWith(".csv") ? csvParse(source) : JSON.parse(source));
      text("import-result", { validated_records: preview.length, preview });
    });
    on("import-commit", async () => {
      if (!preview.length) throw new Error("Validate or prepare a preview first");
      const items = preview;
      if (!await ctx.confirm(`Commit ${items.length} records in batches of 20? Existing IDs will be replaced. Earlier batches remain committed if a later batch fails.`)) return;
      preview = [];
      await commit(items.map((item) => ({ op: "put", ...item })));
    });
    for (const format of ["json", "csv"]) on(`export-page-${format}`, async () => {
      const items = await records(pageIds());
      download(`vlite-page.${format}`, format === "csv" ? csvExport(items) : items, format === "csv" ? "text/csv" : "application/json");
    });
    on("export-hits", async () => {
      const hits = [...$("hits").querySelectorAll("tbody tr")].map((row) => ({ id: row.cells[0].textContent, distance: Number(row.cells[1].textContent) }));
      if (!hits.length) throw new Error("Run a search first");
      download("vlite-search.json", { metric: $("hits").caption.textContent, hits });
    });
    on("batch-delete", async () => {
      const ids = selectedIds();
      if (!await ctx.confirm(`Delete ${ids.length} selected records? This cannot be undone. Batches of 20 are atomic; the whole selection is not.`)) return;
      await commit(ids.map((value) => ({ op: "delete", id: value })));
    });
    on("batch-replace", async () => {
      const ids = selectedIds(), replacement = metadata(get("batch-metadata"));
      if (!await ctx.confirm(`Replace entire metadata maps on ${ids.length} records? Records are read then rewritten, not compare-and-swap; use a dedicated test database without concurrent writers.`)) return;
      const items = await records(ids);
      await commit(items.map((item) => ({ op: "put", ...record({ ...item, metadata: replacement }, ctx.dimensions()) })));
    });
    on("filter-add", async () => {
      const field = $("filter-field").value, type = $("filter-type").value, source = $("filter-value").value, op = $("filter-op").value;
      const value = type === "string" || type === "integer" ? source : JSON.parse(source);
      metadata({ [field]: { type, value } });
      if (op !== "eq" && !["integer", "float"].includes(type)) throw new Error("Range bounds require integer or float values");
      const conditions = get("filter");
      if (!Array.isArray(conditions)) throw new Error("Current filter must be an array");
      conditions.push(op === "eq" ? { op, field, value: { type, value } } : { op: "range", field, [op]: { type, value } });
      $("filter").value = JSON.stringify(conditions, null, 2);
      $("mode").value = "exact"; $("mode").dispatchEvent(new Event("change"));
    });
    on("filter-clear", async () => { $("filter").value = "[]"; });
    on("find-similar", async () => {
      const item = await fetchRecord($("id").value);
      $("query").value = JSON.stringify(item.vector);
      $("filter").value = "[]"; $("mode").value = "exact"; $("mode").dispatchEvent(new Event("change"));
      await ctx.search();
      $("search").scrollIntoView({ block: "start" });
    });
    on("compare-search", async () => {
      if ($("metric").value === "inner_product") throw new Error("ANN comparison supports squared L2 and cosine only");
      const filter = get("filter");
      if (!Array.isArray(filter) || filter.length) throw new Error("Clear filters before unfiltered exact/ANN comparison");
      text("search-comparison", "Running exact then ready-only ANN; the two reads are not a snapshot");
      const body = { vector: vector(get("query"), ctx.dimensions()), k: Number($("k").value), metric: $("metric").value, config: ctx.indexConfig() };
      const started = performance.now();
      const exact = await ctx.api("POST", "/v1/search", { ...body, mode: "exact" });
      const exactMs = performance.now() - started, annStart = performance.now();
      const ann = await ctx.api("POST", "/v1/search", { ...body, mode: "approximate_ready", ef_search: Number($("ef").value) });
      const common = exact.hits.filter((hit) => ann.hits.some((other) => other.id === hit.id)).length;
      text("search-comparison", { exact_ms: exactMs, ann_ms: performance.now() - annStart, common_ids: common, exact_count: exact.hits.length, overlap_fraction: exact.hits.length ? common / exact.hits.length : null, note: "Browser timing; overlap on this query, not a general recall benchmark", exact, ann });
    });
    on("compare-records", async () => {
      const [left, right] = await records([$("id").value, $("compare-id").value]);
      text("record-comparison", { distances: distances(left.vector, right.vector), cosine_note: "null for zero vectors", left, right });
    });
    on("vector-slice", async () => {
      const values = vector(get("record-vector"), ctx.dimensions()), start = Number($("vector-slice-start").value);
      if (!Number.isInteger(start) || start < 0 || start >= values.length) throw new Error("Slice start is outside the vector");
      text("vector-summary", `${values.length} components; zero vector: ${values.every((v) => v === 0)}`);
      text("vector-slice-result", { start, end_exclusive: Math.min(start + 32, values.length), values: values.slice(start, start + 32) });
    });
    on("analyze-sample", async () => {
      sample = []; $("projection").replaceChildren();
      const page = await ctx.api("GET", "/v1/vectors?limit=32");
      sample = await records(page.items.map((item) => item.id));
      text("sample-analysis", analyze(sample));
      if (sample.length < 2 || sample[0].vector.length > 256) {
        $("projection").textContent = "PCA unavailable: requires at least 2 records with at most 256 dimensions. Sample analysis above remains available.";
        return;
      }
      const points = pca(sample), maxX = Math.max(1e-12, ...points.map((p) => Math.abs(p.x))), maxY = Math.max(1e-12, ...points.map((p) => Math.abs(p.y)));
      const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
      svg.setAttribute("viewBox", "-120 -120 240 240"); svg.setAttribute("role", "group"); svg.setAttribute("aria-label", "Approximate two-dimensional PCA projection");
      points.forEach((point) => {
        const circle = document.createElementNS(svg.namespaceURI, "circle");
        circle.setAttribute("cx", String(point.x / maxX * 100)); circle.setAttribute("cy", String(-point.y / maxY * 100)); circle.setAttribute("r", "4"); circle.setAttribute("fill", "#7365db");
        circle.setAttribute("role", "button"); circle.setAttribute("tabindex", "0"); circle.setAttribute("aria-label", `Inspect record ${point.id}`);
        circle.addEventListener("click", () => ctx.run(() => ctx.inspect(`/v1/vectors/${point.id}`)));
        circle.addEventListener("keydown", (event) => {
          if (event.key === "Enter" || event.key === " ") { event.preventDefault(); ctx.run(() => ctx.inspect(`/v1/vectors/${point.id}`)); }
        });
        const title = document.createElementNS(svg.namespaceURI, "title"); title.textContent = `Record ${point.id}`; circle.append(title); svg.append(circle);
      });
      $("projection").append(svg);
      const caption = document.createElement("p"); caption.textContent = "PCA sample IDs (keyboard-accessible inspection):"; $("projection").append(caption);
      points.forEach((point) => {
        const button = document.createElement("button"); button.type = "button"; button.textContent = point.id;
        button.addEventListener("click", () => ctx.run(() => ctx.inspect(`/v1/vectors/${point.id}`))); $("projection").append(button);
      });
    });
    function requestSpec() {
      const method = $("request-method").value, path = $("request-path").value;
      const allowed = method === "GET" ? /^\/(?:health|ready|metrics|openapi\.json|v1\/database|v1\/capabilities|v1\/index\/hnsw|v1\/vectors(?:\/(?:0|[1-9][0-9]*))?)$/
        : method === "PUT" || method === "DELETE" ? /^\/v1\/vectors\/(?:0|[1-9][0-9]*)$/
          : /^\/v1\/(?:search|batches|index\/hnsw)$/;
      if (!allowed.test(path)) throw new Error("Playground path/method is not a supported route; no external origins, queries or credentials allowed");
      return { method, path, ...(method === "GET" || method === "DELETE" ? {} : { body: get("request-body") }) };
    }
    on("send-request", async () => {
      const spec = requestSpec();
      if (spec.method !== "GET" && spec.path !== "/v1/search" && !await ctx.confirm(`Send ${spec.method} ${spec.path}? This can durably change data; no automatic retry.`)) return;
      await ctx.api(spec.method, spec.path, spec.body);
    });
    on("request-examples", async () => {
      const spec = requestSpec(), body = spec.body === undefined ? "" : JSON.stringify(spec.body);
      const quote = (v) => "'" + v.replaceAll("'", "'\\''") + "'";
      text("code-examples", `# Set VLITE_TOKEN locally; never paste credentials into examples.\ncurl -X ${spec.method} -H "Authorization: Bearer $VLITE_TOKEN" -H 'Content-Type: application/json'${body ? " --data " + quote(body) : ""} "$VLITE_BASE${spec.path}"\n\n# Python\nimport os, json, urllib.request\nbody = ${JSON.stringify(body)}\nreq = urllib.request.Request(os.environ["VLITE_BASE"] + ${JSON.stringify(spec.path)}, data=body.encode() if body else None, method=${JSON.stringify(spec.method)}, headers={"Authorization": "Bearer " + os.environ["VLITE_TOKEN"], "Content-Type": "application/json"})\nprint(urllib.request.urlopen(req).read().decode())\n\n// JavaScript / Node.js\nconst response = await fetch(process.env.VLITE_BASE + ${JSON.stringify(spec.path)}, { method: ${JSON.stringify(spec.method)}, headers: { Authorization: "Bearer " + process.env.VLITE_TOKEN, "Content-Type": "application/json" }${body ? ", body: " + JSON.stringify(body) : ""} });\nconsole.log(response.status, await response.text());`);
    });
    function scenario() {
      const value = get("scenario");
      if (!object(value) || Object.keys(value).some((key) => !["name", "steps"].includes(key)) || typeof value.name !== "string" || !Array.isArray(value.steps) || !value.steps.length || value.steps.length > 20) throw new Error("Scenario requires a name and 1..20 steps");
      for (const step of value.steps) {
        if (!object(step) || Object.keys(step).some((key) => !["path", "body", "expected_ids"].includes(key)) || !Array.isArray(step.expected_ids)) throw new Error("Invalid scenario step; credentials/headers are not allowed");
        step.expected_ids.forEach(id);
        if (step.path === "/v1/search") {
          if (!object(step.body) || Object.keys(step.body).some((key) => !["vector", "k", "mode", "metric", "filter", "ef_search"].includes(key))) throw new Error("Invalid scenario search body");
          vector(step.body.vector, ctx.dimensions());
        } else if (!/^\/v1\/vectors\/(?:0|[1-9][0-9]*)$/.test(step.path) || step.body !== undefined) throw new Error("Only record GET and search steps are allowed");
      }
      return value;
    }
    on("save-scenario", async () => download("vlite-scenario.json", scenario()));
    $("scenario-file").addEventListener("change", () => ctx.run(async () => {
      $("scenario").value = await readFile($("scenario-file").files[0]); scenario();
    }));
    on("run-scenario", async () => {
      const value = scenario(), results = [];
      text("scenario-result", "Running read-only scenario");
      for (const step of value.steps) {
        const result = await ctx.api(step.path === "/v1/search" ? "POST" : "GET", step.path, step.body);
        const actual = step.path === "/v1/search" ? result.hits.map((hit) => hit.id) : [result.id];
        results.push({ path: step.path, expected: step.expected_ids, actual, passed: JSON.stringify(actual) === JSON.stringify(step.expected_ids) });
        text("scenario-result", { name: value.name, results });
      }
    });
    async function health() {
      const metrics = await ctx.api("GET", "/metrics");
      const index = await ctx.api("GET", "/v1/index/hnsw");
      text("health-result", { observed_at: new Date().toISOString(), metrics, index, queue_note: "Request admission is bounded; this is a snapshot, not live telemetry" });
    }
    on("refresh-health", health);
    function admin(feature) {
      const caps = ctx.capabilities();
      if (!caps || caps.role !== "admin") throw new Error("HTTP 403: admin credential required");
      if (feature && !caps.features?.[feature]) throw new Error(`Server feature ${feature} is not enabled; inspect compatibility/configuration`);
    }
    on("status-rebuild", async () => text("rebuild-result", await ctx.api("GET", "/v1/index/hnsw/rebuild")));
    on("start-rebuild", async () => {
      admin("background_rebuild");
      if (!await ctx.confirm("Start a bounded background rebuild? Snapshot capture briefly owns the database; worker memory is additional to the active graph.")) return;
      text("rebuild-result", await ctx.api("POST", "/v1/index/hnsw/rebuild", { config: ctx.indexConfig(), metric: $("metric").value, limits: {
        max_records: Number($("hnsw-max-records").value), max_snapshot_vector_bytes: Number($("hnsw-max-bytes").value),
      } }));
    });
    on("poll-rebuild", async () => {
      admin("background_rebuild");
      text("rebuild-result", await ctx.api("POST", "/v1/index/hnsw/rebuild/poll", {}));
    });
    on("cancel-rebuild", async () => {
      admin("background_rebuild");
      if (!await ctx.confirm("Request cooperative cancellation? The current active graph is preserved; poll to reap the worker.")) return;
      text("rebuild-result", await ctx.api("POST", "/v1/index/hnsw/rebuild/cancel", {}));
    });
    on("read-maintenance", async () => {
      const result = await ctx.api("GET", "/v1/index/hnsw/maintenance");
      $("maintenance-policy").value = JSON.stringify(result.policy, null, 2);
      text("maintenance-result", result);
    });
    on("save-maintenance", async () => {
      admin("maintenance");
      const policy = get("maintenance-policy");
      if (!await ctx.confirm("Replace the entire process-local maintenance policy? Omitted fields use defaults, not current values. No timer is started.")) return;
      text("maintenance-result", await ctx.api("PUT", "/v1/index/hnsw/maintenance", policy));
    });
    on("tick-maintenance", async () => {
      admin("maintenance");
      if (!await ctx.confirm("Run one maintenance tick? It may publish a finished graph or start a background rebuild.")) return;
      text("maintenance-result", await ctx.api("POST", "/v1/index/hnsw/maintenance/tick", {}));
    });
    on("token-status", async () => {
      admin(); text("token-result", await ctx.api("GET", "/v1/admin/tokens"));
    });
    on("token-reload", async () => {
      admin("tokens_reload");
      if (!await ctx.confirm("Reload only configured secret files? Invalid files leave credentials unchanged. If the admin token changes, reconnect using the new token.")) return;
      const result = await ctx.api("POST", "/v1/admin/tokens/reload", {});
      ctx.disconnect();
      text("token-result", result);
      ctx.status("Configured credentials reloaded. Reconnect with the current admin/read-only token.");
    });
    on("token-revoke", async () => {
      admin();
      if (!await ctx.confirm("Revoke the in-memory read-only credential now? Persist by emptying its configured file offline; already admitted requests may finish.")) return;
      text("token-result", await ctx.api("POST", "/v1/admin/tokens/revoke-read-only", {}));
    });
    on("read-audit", async () => { admin("audit"); text("audit-result", await ctx.api("GET", "/v1/admin/audit")); });
    async function listBackups() {
      const result = await ctx.api("GET", "/v1/admin/backups");
      for (const [name, kind] of [["backup-name", "backup"], ["restore-name", "restore"]]) {
        $(name).replaceChildren();
        for (const item of result.items.filter((item) => item.kind === kind)) {
          const option = document.createElement("option"); option.value = item.name; option.textContent = `${item.name} (${item.bytes} bytes)`; $(name).append(option);
        }
      }
      text("backup-result", result);
    }
    on("list-backups", async () => { admin(); await listBackups(); });
    on("create-backup", async () => {
      admin("backup_restore");
      if (!await ctx.confirm("Create a consistent full database snapshot in the configured admin directory? This consumes disk and blocks the owner during copying.")) return;
      const result = await ctx.api("POST", "/v1/admin/backups", {});
      await listBackups(); text("backup-result", result);
    });
    on("stage-restore", async () => {
      admin("backup_restore");
      const backup = $("backup-name").value;
      if (!backup) throw new Error("List snapshots and select a generated backup first");
      if (!await ctx.confirm(`Validate and stage ${backup} as a new restored copy? No live file is replaced.`)) return;
      const result = await ctx.api("POST", "/v1/admin/restores", { backup });
      await listBackups(); text("backup-result", result);
    });
    on("select-restore", async () => {
      admin("backup_restore");
      const name = $("restore-name").value;
      if (!name) throw new Error("Stage and select a restored copy first");
      if (!await ctx.confirm(`Select ${name} for the NEXT API restart? Live data will not change now. The persistent selection overrides the positional database until changed offline or another restored copy is selected.`)) return;
      text("backup-result", await ctx.api("POST", "/v1/admin/restores/select", { name }));
    });
    const timer = setInterval(() => {
      if (!$("auto-health").checked || document.hidden || ctx.busy() || !ctx.connected() || refreshing) return;
      refreshing = true;
      ctx.run(health).finally(() => { refreshing = false; });
    }, 10000);
    window.addEventListener("pagehide", () => clearInterval(timer), { once: true });
    function appearance() {
      document.documentElement.dataset.theme = $("theme").value;
      document.documentElement.dataset.density = $("density").value;
      if ($("remember-preferences").checked) localStorage.setItem("vlite-appearance", JSON.stringify({ theme: $("theme").value, density: $("density").value }));
      else localStorage.removeItem("vlite-appearance");
    }
    for (const key of ["theme", "density", "remember-preferences"]) $(key).addEventListener("change", appearance);
    try {
      const saved = localStorage.getItem("vlite-appearance");
      if (saved) {
        const value = JSON.parse(saved);
        if (["light", "dark"].includes(value.theme) && ["compact", "comfortable"].includes(value.density)) {
          $("theme").value = value.theme; $("density").value = value.density; $("remember-preferences").checked = true; appearance();
        }
      }
    } catch (error) { ctx.status(`Appearance preferences could not be loaded: ${error.message}`, true); }
    return {
      clear() {
        preview = []; sample = []; $("auto-health").checked = false; $("projection").replaceChildren();
        for (const name of ["import-result", "sample-analysis", "record-comparison", "search-comparison", "scenario-result", "health-result", "code-examples", "payload-preview", "vector-slice-result", "vector-summary", "compatibility", "rebuild-result", "maintenance-result", "token-result", "audit-result", "backup-result"]) text(name, "");
        for (const name of ["batch-ids", "request-body", "scenario", "filter-value", "import-file", "scenario-file"]) $(name).value = "";
        $("batch-metadata").value = "{}";
        $("backup-name").replaceChildren(); $("restore-name").replaceChildren();
      },
      connected(caps) {
        text("admin-note", `Role: ${caps.role}; API v${caps.api_version}. Backup/restore ${caps.features?.backup_restore ? "enabled" : "disabled: configure VLITE_ADMIN_DIR"}. Token-file reload ${caps.features?.tokens_reload ? "available" : "unavailable: use configured files or restart"}.`);
        $("scenario").value = JSON.stringify({ name: "get-record", steps: [{ path: "/v1/vectors/1", expected_ids: ["1"] }] }, null, 2);
        $("request-body").value = "{}";
      },
      loaded(item) {
        text("vector-summary", `${item.vector.length} components. Inspect a 32-component slice below; full vector remains editable.`);
        try {
          const bytes = Uint8Array.from(atob(item.payload_base64), (char) => char.charCodeAt(0));
          const decoded = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
          try { text("payload-preview", JSON.parse(decoded)); } catch { text("payload-preview", decoded || "(empty payload)"); }
        } catch { text("payload-preview", "Binary payload: use the base64 field. No UTF-8 preview available."); }
      },
    };
  }
  window.VLiteTools = { init, utils: { id, vector, metadata, record, csvParse, csvExport, distances, analyze, pca } };
})();
