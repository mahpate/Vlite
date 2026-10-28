"use strict";
(() => {
  const $ = (id) => document.getElementById(id);
  const object = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
  function responseSummary(value) {
    if (Array.isArray(value)) return `Response contains ${value.length} item${value.length === 1 ? "" : "s"}. Raw JSON details follow.`;
    if (!object(value)) return "";
    const parts = [];
    if (Number.isInteger(value.valid) && Number.isInteger(value.invalid)) parts.push(`Preview: ${value.valid} valid, ${value.invalid} invalid.`);
    for (const key of ["hits", "items", "records", "entries", "results"]) {
      if (Array.isArray(value[key])) {
        const label = key === "hits" ? "hit" : key === "records" ? "record" : "item";
        parts.push(`${value[key].length} ${label}${value[key].length === 1 ? "" : "s"} returned.`);
        break;
      }
    }
    if (!parts.length && typeof value.id === "string") parts.push(`Record ${value.id} returned.`);
    if (!parts.length && typeof value.state === "string") {
      const committed = Number.isInteger(value.rows_committed) ? `${value.rows_committed} rows committed` :
        Number.isInteger(value.last_chunk) ? `${value.last_chunk} chunks committed` : "";
      parts.push(`Job state: ${value.state}${committed ? `; ${committed}` : ""}.`);
    }
    if (!parts.length) parts.push(`Response received with ${Object.keys(value).length} field${Object.keys(value).length === 1 ? "" : "s"}.`);
    return `${parts.join(" ")} Raw JSON details follow.`;
  }
  const responseSummaries = new WeakMap();
  const text = (id, value) => {
    const element = $(id);
    const isRawOutput = element?.tagName === "PRE";
    const summary = isRawOutput && (object(value) || Array.isArray(value)) ? responseSummary(value) : "";
    let summaryElement = responseSummaries.get(element);
    if (summary && element.parentNode) {
      if (!summaryElement) {
        summaryElement = document.createElement("p");
        summaryElement.className = "console-result-summary";
        summaryElement.setAttribute("role", "status");
        summaryElement.setAttribute("aria-live", "polite");
        summaryElement.setAttribute("aria-atomic", "true");
        element.parentNode.insertBefore(summaryElement, element);
        responseSummaries.set(element, summaryElement);
      }
      summaryElement.textContent = summary;
    } else if (summaryElement) {
      summaryElement.remove();
      responseSummaries.delete(element);
    }
    element.textContent = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  };
  const table = (name, rows) => {
    const body = $(name).querySelector("tbody");
    body.replaceChildren();
    for (const cells of rows) {
      const row = document.createElement("tr");
      for (const value of cells) {
        const cell = document.createElement("td");
        if (value instanceof Node) cell.append(value); else cell.textContent = value === null || value === undefined ? "--" : String(value);
        row.append(cell);
      }
      body.append(row);
    }
  };
  function revealAndScroll(target) {
    const navigation = window.VLiteNavigation;
    if (navigation?.reveal) navigation.reveal(target);
    else if (location.hash !== `#${target}`) location.hash = target;
    const element = $(target);
    element?.scrollIntoView({ block: "start", behavior: "instant" });
    return element;
  }
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
  // ---- studio ux: bounded document previews and SDK examples ----
  function jsonDocuments(source, dimensions) {
    if (!Number.isInteger(dimensions) || dimensions < 1) throw new Error("Sign in to discover collection dimensions before validating");
    if (typeof source !== "string" || new TextEncoder().encode(source).length > 4 * 1024 * 1024) throw new Error("JSON documents exceed 4 MiB");
    const values = JSON.parse(source);
    if (!Array.isArray(values) || !values.length || values.length > 10000) throw new Error("Paste an array of 1..10000 documents");
    const records = [], errors = [], seen = new Set();
    let invalid = 0;
    values.forEach((value, index) => {
      try {
        if (!object(value)) throw new Error("Expected a document object");
        if (value.payload !== undefined && value.payload_base64 !== undefined) throw new Error("Use payload OR payload_base64, not both");
        const { payload, ...fields } = value;
        const item = record(payload === undefined ? fields : { ...fields, payload_base64: payload }, dimensions);
        if (seen.has(item.id)) throw new Error(`Duplicate ID ${item.id}`);
        seen.add(item.id); records.push(item);
      } catch (error) {
        invalid++;
        if (errors.length < 20) errors.push(`Document ${index + 1}: ${error.message}`);
      }
    });
    return { count: values.length, valid: records.length, invalid, errors, records: invalid ? [] : records };
  }
  function collectionDraft(input) {
    const initial = settings(JSON.parse(input.settings || "{}"));
    const declared = { ...initial };
    if (input.metric) declared.metric = input.metric;
    if (input.indexes?.trim()) declared.metadata_indexes = JSON.parse(input.indexes);
    return { name: collectionName(input.name.trim()), dimensions: boundedInteger(input.dimensions, 1, 65536, "Dimensions"), settings: settings(declared) };
  }
  function codeSnippet({ kind, collection, body, language, legacy = false, recordId }) {
    collectionName(collection);
    if (!["search", "upsert", "write"].includes(kind)) throw new Error("Unsupported code operation");
    if (!["curl", "python", "javascript"].includes(language)) throw new Error("Unsupported code language");
    if (kind === "upsert") id(recordId);
    const prefix = collection === "default" && legacy ? "/v1" : `/v1/collections/${collection}`;
    const suffix = kind === "search" ? "/search" : kind === "write" ? "/batches" : `/vectors/${recordId}`;
    const method = kind === "upsert" ? "PUT" : "POST";
    const bodies = kind === "write" ? chunk(body.operations, 20).map((operations) => ({ operations })) : [body];
    if (!bodies.length) throw new Error("No operations to generate");
    const json = (value) => JSON.stringify(redact(value));
    const shell = (value) => "'" + value.replaceAll("'", "'\\''") + "'";
    const target = collection === "default" && legacy ? "client" : `client.collection(${JSON.stringify(collection)})`;
    if (language === "curl") {
      return '# POSIX shell. Set VLITE_BASE and VLITE_TOKEN locally; no token is embedded.\n# Stop on any failure; verify unknown outcomes before retrying.\n' + bodies.map((item) =>
        `curl --fail-with-body -X ${method} -H "Authorization: Bearer $VLITE_TOKEN" -H 'Content-Type: application/json' --data-binary ${shell(json(item))} "$VLITE_BASE${prefix}${suffix}" || exit 1`).join("\n");
    }
    if (language === "python") {
      const calls = bodies.map((item) => {
        const argument = `json.loads(${JSON.stringify(json(kind === "write" ? item.operations : item))})`;
        return kind === "search" ? `print(records.search_detailed(${argument}))` : kind === "write" ? `records.batch(${argument})` : `records.put(${JSON.stringify(recordId)}, ${argument})`;
      }).join("\n");
      return `# pip install vlite-client; set VLITE_BASE and VLITE_TOKEN locally.\nimport os, json\nfrom vlite_client import Client\nclient = Client(os.environ["VLITE_BASE"], os.environ["VLITE_TOKEN"])\nrecords = ${target}\n# No retries; earlier batches remain committed on failure.\n${calls}`;
    }
    const calls = bodies.map((item) => {
      const argument = json(kind === "write" ? item.operations : item);
      return kind === "search" ? `console.log(await records.searchDetailed(${argument}));` : kind === "write" ? `await records.batch(${argument});` : `await records.put(${JSON.stringify(recordId)}, ${argument});`;
    }).join("\n");
    return `// Node.js ES module; use clients/javascript (@vlite/client) from this repository.\nimport { Client } from "@vlite/client";\nconst client = new Client(process.env.VLITE_BASE, process.env.VLITE_TOKEN);\nconst records = ${target};\n// No retries; earlier batches remain committed on failure.\n${calls}`;
  }
  function savedQuery(value, dimensions) {
    if (!object(value) || Object.keys(value).sort().join(",") !== "name,query,version" || value.version !== 1 ||
        typeof value.name !== "string" || !value.name.trim() || value.name.length > 100) throw new Error("Expected a version 1 named query file without extra fields");
    const q = value.query;
    if (!object(q) || Object.keys(q).sort().join(",") !== "config,ef_search,filter,k,metric,mode,vector") throw new Error("Unknown or missing query fields; credentials are not accepted");
    vector(q.vector, dimensions);
    if (!Number.isInteger(q.k) || q.k < 0 || q.k > 10000 || !Number.isInteger(q.ef_search) || q.ef_search < 1 || q.ef_search > 100000 ||
        !["exact", "approximate", "approximate_ready"].includes(q.mode) || !["squared_l2", "cosine", "inner_product"].includes(q.metric) ||
        (q.mode !== "exact" && q.metric === "inner_product")) throw new Error("Invalid query search settings");
    if (!object(q.config) || Object.keys(q.config).sort().join(",") !== "ef_construction,m,seed" ||
        !Number.isInteger(q.config.m) || q.config.m < 2 || q.config.m > 64 ||
        !Number.isInteger(q.config.ef_construction) || q.config.ef_construction < q.config.m || q.config.ef_construction > 4096) throw new Error("Invalid query HNSW configuration");
    id(q.config.seed);
    if (!Array.isArray(q.filter) || q.filter.length > 64) throw new Error("Expected up to 64 query filter conditions");
    for (const condition of q.filter) {
      if (!object(condition) || typeof condition.field !== "string") throw new Error("Invalid query filter condition");
      if (condition.op === "eq" && Object.keys(condition).sort().join(",") === "field,op,value") {
        metadata(Object.fromEntries([[condition.field, condition.value]]));
      } else if (condition.op === "range" && Object.keys(condition).every((key) => ["op", "field", "min", "max"].includes(key))) {
        const bounds = [condition.min, condition.max].filter((bound) => bound !== null && bound !== undefined);
        if (!bounds.length) throw new Error("Range filters require at least one bound");
        for (const bound of bounds) {
          metadata(Object.fromEntries([[condition.field, bound]]));
          if (!["integer", "float"].includes(bound.type)) throw new Error("Range filters require numeric values");
        }
        if (bounds.length === 2 && (bounds[0].type !== bounds[1].type ||
            (bounds[0].type === "integer" ? BigInt(bounds[0].value) > BigInt(bounds[1].value) : bounds[0].value > bounds[1].value))) throw new Error("Range bounds must have matching types and minimum <= maximum");
      } else throw new Error("Invalid query filter condition");
    }
    if (new TextEncoder().encode(JSON.stringify(value)).length > 4 * 1024 * 1024) throw new Error("Query exceeds 4 MiB");
    return value;
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
    if (items.length > 2000) throw new Error("PCA is limited to 2000 sampled records");
    const dimensions = items[0].vector.length;
    if (dimensions > 256) throw new Error("PCA visualization is limited to 256 dimensions");
    items.forEach((item) => vector(item.vector, dimensions));
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
  // ---- EPIC-015 helpers: collections, search modes, imports, credentials ----
  const COLLECTION_NAME = /^[a-z][a-z0-9_-]{0,62}$/;
  function collectionName(value) {
    if (typeof value !== "string" || !COLLECTION_NAME.test(value)) throw new Error("Collection names match ^[a-z][a-z0-9_-]{0,62}$");
    return value;
  }
  // `default` keeps the legacy routes so older servers and existing workflows are unchanged.
  function collectionPath(collection, suffix) {
    collectionName(collection);
    if (!suffix.startsWith("/")) throw new Error("Collection route suffix must start with /");
    return collection === "default" ? `/v1${suffix}` : `/v1/collections/${collection}${suffix}`;
  }
  const METRICS = ["squared_l2", "cosine", "inner_product"];
  const INDEX_KINDS = ["eq", "range", "eq_range"];
  function settings(value) {
    if (!object(value)) throw new Error("Settings must be a JSON object");
    const allowed = ["metric", "hnsw", "quantization", "rerank_multiplier", "planner", "metadata_indexes", "text_fields"];
    const unknown = Object.keys(value).filter((key) => !allowed.includes(key));
    if (unknown.length) throw new Error(`Unknown settings fields: ${unknown.join(", ")}`);
    if (value.metric !== undefined && !METRICS.includes(value.metric)) throw new Error("metric must be squared_l2, cosine or inner_product");
    if (value.hnsw !== undefined) {
      const h = value.hnsw;
      if (!object(h) || Object.keys(h).some((key) => !["m", "ef_construction", "seed"].includes(key)) ||
          !Number.isInteger(h.m) || h.m < 2 || h.m > 64 || !Number.isInteger(h.ef_construction) || h.ef_construction < h.m || h.ef_construction > 4096) throw new Error("hnsw requires m 2..64 and ef_construction m..4096");
      id(h.seed);
    }
    if (value.quantization !== undefined && !["none", "scalar_u8"].includes(value.quantization)) throw new Error("quantization must be none or scalar_u8");
    if (value.rerank_multiplier !== undefined && (!Number.isInteger(value.rerank_multiplier) || value.rerank_multiplier < 1 || value.rerank_multiplier > 16)) throw new Error("rerank_multiplier must be 1..16");
    if (value.planner !== undefined) {
      const p = value.planner;
      if (!object(p) || Object.keys(p).some((key) => !["exact_max_candidates", "exact_max_selectivity_ppm"].includes(key)) ||
          (p.exact_max_candidates !== undefined && (!Number.isInteger(p.exact_max_candidates) || p.exact_max_candidates < 1 || p.exact_max_candidates > 1000000)) ||
          (p.exact_max_selectivity_ppm !== undefined && (!Number.isInteger(p.exact_max_selectivity_ppm) || p.exact_max_selectivity_ppm < 0 || p.exact_max_selectivity_ppm > 1000000))) throw new Error("planner requires exact_max_candidates 1..1000000 and exact_max_selectivity_ppm 0..1000000");
    }
    const field = (name) => typeof name === "string" && name.length > 0 && new TextEncoder().encode(name).length <= 256;
    if (value.metadata_indexes !== undefined) {
      if (!Array.isArray(value.metadata_indexes) || value.metadata_indexes.length > 16) throw new Error("At most 16 metadata indexes");
      for (const entry of value.metadata_indexes) {
        if (!object(entry) || Object.keys(entry).sort().join(",") !== "field,kind" || !field(entry.field) || !INDEX_KINDS.includes(entry.kind)) throw new Error("Metadata indexes are {field, kind: eq|range|eq_range}");
      }
      if (new Set(value.metadata_indexes.map((entry) => entry.field)).size !== value.metadata_indexes.length) throw new Error("Duplicate metadata index field");
    }
    if (value.text_fields !== undefined) {
      if (!Array.isArray(value.text_fields) || value.text_fields.length > 8 || !value.text_fields.every(field)) throw new Error("At most 8 text fields of 1..256 bytes");
      if (new Set(value.text_fields).size !== value.text_fields.length) throw new Error("Duplicate text field");
    }
    return value;
  }
  // Higher-is-better scores must never be presented as distances.
  function scoreDirection(mode, metric) {
    if (mode === "hybrid") return { column: "score", label: "Hybrid RRF score (higher is better)", better: "higher" };
    return { column: "distance", label: `${metric ?? "squared_l2"} distance (lower is better)`, better: "lower" };
  }
  const SECRET_KEY = /^(?:token|secret|authorization|password|api_key|bearer)$|_token$|_secret$|_password$/i;
  const SECRET_TEXT = /vlk_[a-z2-7]{12}_[A-Za-z0-9_-]{20,}/g;
  // Deep copy for display/export surfaces: removes credential-looking fields and scoped tokens.
  function redact(value, depth = 0) {
    if (depth > 32) return "[redacted: too deep]";
    if (typeof value === "string") return value.replace(SECRET_TEXT, "[redacted credential]");
    if (Array.isArray(value)) return value.map((item) => redact(item, depth + 1));
    if (!object(value)) return value;
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, SECRET_KEY.test(key) && item !== null && typeof item !== "boolean" ? "[redacted]" : redact(item, depth + 1)]));
  }
  function maskSecret(token) {
    if (typeof token !== "string" || !token) return "";
    const match = /^(vlk_[a-z2-7]{12}_)(.+)$/.exec(token);
    return match ? `${match[1]}${"\u2022".repeat(12)} (${match[2].length}-character secret hidden)` : `${"\u2022".repeat(12)} (${token.length}-character secret hidden)`;
  }
  const OPERATIONS = ["read", "search", "search_build", "write", "import", "index", "collections", "diagnostics", "backup", "graph_read", "graph_write"];
  function optionalLimit(value, min, max, label) {
    if (value === undefined || (typeof value === "string" && !value.trim())) return undefined;
    const source = typeof value === "string" ? value.trim() : value;
    if (source === null || source === "null") return null;
    if ((typeof source !== "number" && typeof source !== "string") ||
        (typeof source === "string" && !/^(0|[1-9][0-9]*)$/.test(source))) throw new Error(`${label} must be a decimal integer or null (no limit)`);
    const number = Number(source);
    if (!Number.isSafeInteger(number) || number < min || number > max) throw new Error(`${label} must be ${min}..${max}, or null (no limit); larger integers cannot be represented exactly by this dashboard`);
    return number;
  }
  function credentialRequest(input, now = Date.now()) {
    const operations = [...new Set(input.operations)];
    if (!operations.length || operations.some((value) => !OPERATIONS.includes(value))) throw new Error(`Select at least one operation from ${OPERATIONS.join(", ")}`);
    const graphOnly = operations.every((value) => value === "graph_read" || value === "graph_write");
    const collections = graphOnly || input.collections.trim() === "*" ? ["*"] : commaList(input.collections);
    if (!collections.length || collections.length > 64) throw new Error("Scope 1..64 collections, or * for all");
    if (!(collections.length === 1 && collections[0] === "*")) collections.forEach(collectionName);
    if (new Set(collections).size !== collections.length) throw new Error("Duplicate collection scope");
    const days = Number(input.days);
    if (!Number.isInteger(days) || days < 1 || days > 365) throw new Error("Expiry must be 1..365 days; credentials always expire");
    const body = { collections, operations, expires_unix_ms: now + days * 86400000 };
    if (typeof input.name !== "string" || input.name.length < 1 || input.name.length > 64 || !/^[\x20-\x7e]+$/.test(input.name)) throw new Error("Credential name is required: 1..64 printable ASCII characters");
    body.name = input.name;
    if (input.tenant_id !== undefined && input.tenant_id !== "") {
      const tenant = typeof input.tenant_id === "string" ? input.tenant_id.trim() : input.tenant_id;
      if (tenant === null || tenant === "null") body.tenant_id = null;
      else if (tenant !== "") {
        if (typeof tenant !== "string" || !/^[a-fA-F0-9]{32}$/.test(tenant)) throw new Error("Tenant must be an existing 32-character hexadecimal tenant ID, or blank for no tenant");
        body.tenant_id = tenant;
      }
    }
    const graphs = commaList(input.graphs);
    const graphOps = operations.some((value) => value === "graph_read" || value === "graph_write");
    if (graphOps) {
      if (!graphs.length || graphs.length > 64) throw new Error("Graph operations require 1..64 graph names, or * for all");
      if (!(graphs.length === 1 && graphs[0] === "*")) {
        if (graphs.some((value) => !GRAPH_NAME.test(value))) throw new Error("Graph names match ^[a-z][a-z0-9_-]{0,62}$; * must be used alone");
        if (body.tenant_id && graphs.some((value) => value.length > 28)) throw new Error("Tenant graph names must be at most 28 characters");
      }
      if (new Set(graphs).size !== graphs.length) throw new Error("Duplicate graph scope");
      body.graphs = graphs;
    } else if (graphs.length) throw new Error("Select graph_read or graph_write to grant access to graphs");
    for (const [key, min, max, label] of [
      ["requests_per_second", 1, 1000000, "Requests per second"],
      ["max_vectors", 0, Number.MAX_SAFE_INTEGER, "Total vector quota"],
      ["max_vectors_per_collection", 0, Number.MAX_SAFE_INTEGER, "Per-collection vector quota"],
    ]) {
      const value = optionalLimit(input[key], min, max, label);
      if (value !== undefined) body[key] = value;
    }
    return body;
  }
  function credentialScope(item) {
    return [`collections [${item.collections.join(", ")}]`, `graphs [${(item.graphs ?? []).join(", ") || "none"}]`,
      item.tenant_id ? `tenant ${item.tenant_id}` : "no tenant",
      `rate ${item.requests_per_second ?? "unlimited"} requests/s`,
      `vectors ${item.max_vectors ?? "unlimited"} total / ${item.max_vectors_per_collection ?? "unlimited"} per collection`].join("; ");
  }
  function graceSeconds(value) {
    const seconds = Number(value);
    if (!Number.isInteger(seconds) || seconds < 0 || seconds > 86400) throw new Error("Grace must be 0..86400 seconds");
    return seconds;
  }
  function boundedInteger(value, min, max, label) {
    const number = Number(value);
    if (!Number.isInteger(number) || number < min || number > max) throw new Error(`${label} must be ${min}..${max}`);
    return number;
  }
  // Parses JSON array, NDJSON/JSONL or CSV into put operations. Structure is checked
  // locally; dimension and value rules are enforced (and reported per row) by the server.
  function importRows(name, source) {
    const lower = name.toLowerCase();
    let rows;
    if (lower.endsWith(".csv")) rows = csvParse(source);
    else if (lower.endsWith(".ndjson") || lower.endsWith(".jsonl")) {
      rows = source.split(/\r?\n/).map((line, index) => {
        if (!line.trim()) return null;
        try { return JSON.parse(line); } catch { throw new Error(`Line ${index + 1}: invalid JSON`); }
      }).filter((row) => row !== null);
    } else rows = JSON.parse(source);
    if (!Array.isArray(rows) || !rows.length) throw new Error("Import file contains no records");
    if (rows.length > 1000000) throw new Error("Browser import is limited to 1,000,000 records");
    return rows.map((row, index) => {
      if (!object(row) || Object.keys(row).some((key) => !["id", "vector", "metadata", "payload_base64"].includes(key))) throw new Error(`Row ${index + 1}: unknown fields; records are {id, vector, payload_base64?, metadata?}`);
      try { id(row.id); } catch (error) { throw new Error(`Row ${index + 1}: ${error.message}`); }
      if (!Array.isArray(row.vector) || !row.vector.every((v) => typeof v === "number")) throw new Error(`Row ${index + 1}: vector must be a numeric array`);
      if (row.metadata !== undefined && !object(row.metadata)) throw new Error(`Row ${index + 1}: metadata must be an object`);
      if (row.payload_base64 !== undefined && typeof row.payload_base64 !== "string") throw new Error(`Row ${index + 1}: payload_base64 must be a string`);
      return { op: "put", id: row.id, vector: row.vector, ...(row.payload_base64 ? { payload_base64: row.payload_base64 } : {}), ...(row.metadata ? { metadata: row.metadata } : {}) };
    });
  }
  // Deterministic chunking: the same file and size always produce identical chunks,
  // which the server's exactly-once digest check relies on when resuming.
  function chunk(rows, size) {
    boundedInteger(size, 1, 4096, "Chunk size");
    const chunks = [];
    for (let start = 0; start < rows.length; start += size) chunks.push(rows.slice(start, start + size));
    for (const [index, item] of chunks.entries()) {
      if (new TextEncoder().encode(JSON.stringify(item)).length > 16 * 1024 * 1024) throw new Error(`Chunk ${index + 1} exceeds 16 MiB; use a smaller chunk size`);
    }
    return chunks;
  }
  // Decide how to continue a job from its durable server status; never guesses.
  function resumePlan(status, totalChunks) {
    if (!object(status)) throw new Error("Import status unavailable");
    const state = status.state, last = Number(status.last_chunk ?? 0);
    if (!Number.isInteger(last) || last < 0) throw new Error("Import status has an invalid last_chunk");
    if (state === "finished" || state === "aborted") return { action: "closed", last, message: `Job is ${state}; ${last} chunk(s) committed and retained. Nothing to resend.` };
    if (state !== "open") throw new Error(`Unknown import state ${state}`);
    if (last > totalChunks) return { action: "mismatch", last, message: `Server committed ${last} chunks but this file has only ${totalChunks}; select the original file and chunk size.` };
    return { action: last === totalChunks ? "finish" : "continue", last, verify: last >= 1 ? last : null, next: last + 1,
      message: last ? `Chunks 1..${last} are durably committed and will not be resent; chunk ${last} is replayed once to prove this is the same file (expects already_committed).` : "No chunks committed yet; starting at chunk 1." };
  }
  function savedQueryV2(value, dimensions, collection) {
    if (!object(value) || value.version !== 2 || Object.keys(value).sort().join(",") !== "collection,name,query,version" ||
        typeof value.name !== "string" || !value.name.trim() || value.name.length > 100) throw new Error("Expected a version 2 named query file without extra fields");
    collectionName(value.collection);
    if (collection !== undefined && value.collection !== collection) throw new Error(`Query belongs to collection "${value.collection}"; select that collection first. Nothing was loaded.`);
    const q = value.query;
    const keys = ["config", "ef_search", "explain", "filter", "fusion", "k", "metric", "mode", "quantization", "rerank_multiplier", "text", "vector", "vector_leg"];
    if (!object(q) || Object.keys(q).some((key) => ![...keys, "rerank"].includes(key)) || keys.some((key) => !(key in q))) throw new Error("Unknown or missing query fields; credentials are not accepted");
    if (q.rerank !== undefined) {
      exactKeys(q.rerank, ["mode", "candidates", "lambda"], ["mode", "candidates"], "Reranking");
      if (!["exact", "mmr"].includes(q.rerank.mode) || typeof q.rerank.candidates !== "number" || ("lambda" in q.rerank && (q.rerank.mode !== "mmr" || typeof q.rerank.lambda !== "number"))) throw new Error("Invalid rerank candidate count or lambda");
      rerank(q.rerank.mode, q.rerank.candidates, q.rerank.lambda ?? 0.5, q.k, q.mode);
    }
    if (!["exact", "approximate", "approximate_ready", "auto", "hybrid"].includes(q.mode)) throw new Error("Invalid query mode");
    if (!["exact", "approximate_ready"].includes(q.vector_leg) || !Number.isInteger(q.rerank_multiplier) || q.rerank_multiplier < 1 || q.rerank_multiplier > 16) throw new Error("Invalid vector_leg or rerank_multiplier");
    if (typeof q.text !== "string" || new TextEncoder().encode(q.text).length > 1024) throw new Error("Query text must be at most 1024 bytes");
    if (q.vector === null) {
      if (q.mode !== "hybrid" || !q.text.trim()) throw new Error("Only hybrid queries with text may omit the vector");
    } else vector(q.vector, dimensions);
    if (typeof q.explain !== "boolean" || !["none", "scalar_u8"].includes(q.quantization)) throw new Error("Invalid explain/quantization fields");
    if (!object(q.fusion) || Object.keys(q.fusion).sort().join(",") !== "candidates,rrf_k,text_weight,vector_weight") throw new Error("Invalid fusion fields");
    boundedInteger(q.fusion.rrf_k, 1, 10000, "fusion.rrf_k");
    boundedInteger(q.fusion.candidates, 1, 10000, "fusion.candidates");
    for (const weight of [q.fusion.vector_weight, q.fusion.text_weight]) if (typeof weight !== "number" || !Number.isFinite(weight) || weight < 0 || weight > 1000) throw new Error("Fusion weights must be 0..1000");
    savedQuery({ version: 1, name: value.name, query: { vector: q.vector ?? [0], k: q.k, mode: ["auto", "hybrid"].includes(q.mode) ? "exact" : q.mode, metric: q.metric, ef_search: q.ef_search, filter: q.filter, config: q.config } }, q.vector === null ? undefined : dimensions);
    return value;
  }
  function exactKeys(value, allowed, required, label) {
    if (!object(value) || Object.keys(value).some((key) => !allowed.includes(key)) || required.some((key) => !(key in value))) throw new Error(`${label}: unknown or missing fields (allowed: ${allowed.join(", ")})`);
  }
  // Partial (merge) bodies for the memory-only schedulers; bounds mirror the server. Omitted fields keep their current value.
  function autoMaintenance(value) {
    const keys = ["enabled", "interval_secs", "stale_ratio", "rebuild_on_low_recall", "min_recall", "quality_samples", "limits"];
    exactKeys(value, keys, [], "Automatic maintenance");
    if (!Object.keys(value).length) throw new Error("Automatic maintenance: change at least one field");
    for (const key of ["enabled", "rebuild_on_low_recall"]) if (key in value && typeof value[key] !== "boolean") throw new Error(`${key} must be a boolean`);
    if ("interval_secs" in value) boundedInteger(value.interval_secs, 10, 3600, "interval_secs");
    if ("quality_samples" in value) boundedInteger(value.quality_samples, 1, 32, "quality_samples");
    if ("stale_ratio" in value && (typeof value.stale_ratio !== "number" || !(value.stale_ratio > 0 && value.stale_ratio <= 1))) throw new Error("stale_ratio must be greater than 0 and at most 1");
    if ("min_recall" in value && (typeof value.min_recall !== "number" || !(value.min_recall >= 0 && value.min_recall <= 1))) throw new Error("min_recall must be 0..1");
    if ("limits" in value && !object(value.limits)) throw new Error("limits must be an object");
    return value;
  }
  // GET returns {config:{interval_ms,...}}; PUT takes interval_secs.
  function autoMaintenanceForm(config) {
    const form = {};
    for (const key of ["enabled", "stale_ratio", "rebuild_on_low_recall", "min_recall", "quality_samples"]) if (key in config) form[key] = config[key];
    if (Number.isFinite(config.interval_ms)) form.interval_secs = Math.round(config.interval_ms / 1000);
    return form;
  }
  function backupSchedule(value) {
    const keys = ["enabled", "interval_secs", "retain_last", "retain_days", "retention_enabled", "drill_every", "drill_max_bytes"];
    exactKeys(value, keys, [], "Backup schedule");
    if (!Object.keys(value).length) throw new Error("Backup schedule: change at least one field");
    for (const key of ["enabled", "retention_enabled"]) if (key in value && typeof value[key] !== "boolean") throw new Error(`${key} must be a boolean`);
    for (const [key, min, max] of [["interval_secs", 300, 604800], ["retain_last", 1, 100], ["retain_days", 0, 3650], ["drill_every", 0, 100]]) if (key in value) boundedInteger(value[key], min, max, key);
    if ("drill_max_bytes" in value && (!Number.isSafeInteger(value.drill_max_bytes) || value.drill_max_bytes < 1)) throw new Error("drill_max_bytes must be a positive integer");
    return value;
  }
  // Index component byte counts. The total is only the sum of these byte components.
  function memoryRows(index) {
    if (!index?.memory) return [];
    const rows = Object.entries(index.memory).filter(([name, bytes]) => name.endsWith("_bytes") && Number.isFinite(bytes));
    rows.push(["sum of listed components (not process memory)", rows.reduce((sum, [, bytes]) => sum + bytes, 0)]);
    return rows;
  }
  const NAME = "[a-z][a-z0-9_-]{0,62}", ID = "(?:0|[1-9][0-9]*)";
  const SCOPED = `(?:/collections/${NAME})?`;
  // REST playground allowlist. Credential creation/rotation is intentionally absent: one-time
  // secrets are only handled by the Scoped credentials panel, never the general inspector.
  function requestAllowed(method, path) {
    const patterns = {
      GET: [`^/(?:health|ready|metrics|openapi\\.json|v1/database|v1/capabilities|v1/collections)$`, `^/v1/collections/${NAME}$`,
        `^/v1${SCOPED}/vectors(?:/${ID})?$`, `^/v1${SCOPED}/index/hnsw(?:/rebuild|/maintenance)?$`, `^/v1/imports/${ID}$`,
        "^/v1/admin/(?:maintenance/auto|backups/schedule|diagnostics/slow-queries|credentials)$"],
      PUT: [`^/v1${SCOPED}/vectors/${ID}$`],
      DELETE: [`^/v1${SCOPED}/vectors/${ID}$`],
      POST: [`^/v1${SCOPED}/(?:search|batches|index/hnsw)$`],
    }[method] || [];
    return patterns.some((pattern) => new RegExp(pattern).test(path));
  }
  const isSearch = (path) => new RegExp(`^/v1${SCOPED}/(?:search(?:/(?:grouped|geo|named|sparse|dense-sparse|disk-ann))?|recommend|facets|stats/numeric|query)$`).test(path);
  function expiry(value) {
    if (value === "") return {};
    if (!/^(0|[1-9][0-9]*)$/.test(String(value))) throw new Error("TTL must be a nonnegative integer in milliseconds");
    return { ttl_ms: boundedInteger(value, 0, Number.MAX_SAFE_INTEGER, "TTL milliseconds") };
  }
  function rerank(mode, candidates, lambda, k, searchMode) {
    if (!mode) return {};
    if (!["exact", "mmr"].includes(mode) || searchMode === "hybrid") throw new Error("Reranking requires vector search and an exact or MMR mode");
    if (String(candidates).trim() === "") throw new Error("Rerank candidates are required");
    const result = { mode, candidates: boundedInteger(candidates, Math.max(k, 1), 1000, "Rerank candidates") };
    if (mode === "mmr") {
      const value = Number(lambda);
      if (lambda === "" || !Number.isFinite(value) || value < 0 || value > 1) throw new Error("MMR lambda must be 0..1");
      result.lambda = value;
    }
    return { rerank: result };
  }
  function sparseVector(value) {
    if (!Array.isArray(value) || value.length > 65536) throw new Error("Expected up to 65536 sparse pairs");
    let previous = -1;
    for (const pair of value) {
      if (!Array.isArray(pair) || pair.length !== 2 || !Number.isInteger(pair[0]) || pair[0] <= previous || pair[0] > 4294967295 || typeof pair[1] !== "number" || !Number.isFinite(pair[1]) || Math.abs(pair[1]) > 3.4028234663852886e38) throw new Error("Sparse entries require increasing unique u32 indices and finite f32 values");
      previous = pair[0];
    }
    return value;
  }
  const WEBHOOK_EVENTS = ["put", "delete", "metadata_update", "collection_created", "collection_dropped", "settings_updated", "vectors_updated", "graph_created", "graph_dropped", "graph_batch"];
  const GRAPH_NAME = /^[a-z][a-z0-9_-]{0,62}$/;
  const commaList = (value) => (value ?? "").split(",").map((v) => v.trim()).filter(Boolean);
  // Blank events, collections or graphs are omitted, so the server applies its default: everything.
  function webhookRequest(input) {
    const url = new URL(input.url);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.hash) throw new Error("Webhook URL must use HTTP(S), without credentials or fragment");
    const secretBytes = new TextEncoder().encode(input.secret).length;
    if (secretBytes < 16 || secretBytes > 256) throw new Error("Signing secret must contain 16..256 bytes");
    const events = commaList(input.events);
    if (new Set(events).size !== events.length || events.some((v) => !WEBHOOK_EVENTS.includes(v))) throw new Error(`Webhook events must be unique and from: ${WEBHOOK_EVENTS.join(", ")}`);
    const collections = commaList(input.collections);
    if (collections.length > 64) throw new Error("Webhook filters accept at most 64 collections");
    collections.forEach(collectionName);
    if (new Set(collections).size !== collections.length) throw new Error("Duplicate webhook collection");
    const graphs = commaList(input.graphs);
    if (graphs.length > 64) throw new Error("Webhook filters accept at most 64 graphs");
    if (graphs.some((v) => !GRAPH_NAME.test(v))) throw new Error("Graph names match ^[a-z][a-z0-9_-]{0,62}$");
    if (new Set(graphs).size !== graphs.length) throw new Error("Duplicate webhook graph");
    return { url: url.href, secret: input.secret, ...(events.length ? { events } : {}), ...(collections.length ? { collections } : {}), ...(graphs.length ? { graphs } : {}) };
  }
  // One row per feed event: sequence, event, collection, graph, details. Graph events carry a graph, not a collection.
  function feedRows(changes) {
    if (!Array.isArray(changes)) return [];
    return changes.map((entry) => {
      const change = object(entry?.change) ? entry.change : {};
      const type = typeof change.type === "string" ? change.type : "unknown";
      const graph = typeof change.graph === "string" ? change.graph : null;
      let details = null;
      if (typeof change.id === "string") details = `Record ${change.id}`;
      else if (Number.isInteger(change.dimensions)) details = `${change.dimensions} dimensions`;
      else if (type === "graph_batch") {
        const operations = Array.isArray(change.operations) ? change.operations : [];
        const counts = new Map();
        for (const operation of operations) {
          const op = typeof operation?.op === "string" ? operation.op : "unknown";
          counts.set(op, (counts.get(op) ?? 0) + 1);
        }
        const summary = [...counts].map(([op, count]) => `${count} ${op}`).join(", ");
        details = `Revision ${change.revision ?? "--"}: ${operations.length} operation${operations.length === 1 ? "" : "s"}${summary ? ` (${summary})` : ""}`;
      }
      const collection = graph === null && typeof entry?.collection === "string" && entry.collection ? entry.collection : null;
      return [entry?.sequence ?? null, type, collection, graph, details];
    });
  }
  function pitrRequest(backup, kind, target) {
    if (!/^[a-zA-Z0-9_.-]{1,96}$/.test(backup) || backup.includes("..")) throw new Error("Use an existing generated backup name, not a path");
    if (!["sequence", "timestamp_ms"].includes(kind) || !/^(0|[1-9][0-9]*)$/.test(target)) throw new Error("Choose a PITR target and a canonical nonnegative integer");
    return { backup, [kind]: boundedInteger(target, 0, Number.MAX_SAFE_INTEGER, "PITR target") };
  }
  function feedPath(cursor, limit) {
    const query = new URLSearchParams({ limit: String(boundedInteger(limit, 1, 10000, "Feed limit")) });
    if (cursor) query.set("cursor", cursor);
    return `/v1/changes?${query}`;
  }
  function advancedTools(ctx, on, get) {
    let nextCursor = null, hasMore = false, draftDirty = false;
    document.querySelectorAll("#retrieval input, #retrieval textarea, #retrieval select").forEach((element) => {
      element.addEventListener("input", () => { draftDirty = true; });
      element.addEventListener("change", () => { draftDirty = true; });
    });
    const route = (suffix) => `/v1/collections/${collectionName(ctx.collection())}${suffix}`;
    const display = (name, value) => text(name, redact(value));
    const field = (name) => {
      const value = $(name).value.trim();
      if (!value || new TextEncoder().encode(value).length > 256) throw new Error("Field/name must contain 1..256 bytes");
      return value;
    };
    const limit = () => boundedInteger($("k").value, 0, 1000, "Result limit");
    const filter = () => {
      const value = get("filter");
      if (!Array.isArray(value) || value.length > 64) throw new Error("Expected up to 64 filter conditions");
      return value;
    };
    const base = () => ({ vector: vector(get("query"), ctx.dimensions()), metric: $("metric").value, filter: filter() });
    async function read(suffix, body, result) {
      if (!ctx.can("search")) throw new Error("HTTP 403: search is not permitted for this collection");
      text(result, "Request pending");
      try { display(result, await ctx.api("POST", route(suffix), body, { data: false })); }
      catch (error) { text(result, "Request failed; see status and request inspector. No results retained."); throw error; }
    }
    on("group-search", () => read("/search/grouped", { ...base(), field: field("group-field"), groups: boundedInteger($("group-count").value, 1, 1000, "Groups"), hits_per_group: boundedInteger($("group-hits").value, 1, 1000, "Hits per group") }, "group-result"));
    const named = () => ({ name: field("named-name"), vector: vector(get("named-query"), null), k: boundedInteger($("named-k").value, 0, 1000, "Result limit"), ...($("named-version").value ? { version: field("named-version") } : {}) });
    on("named-search", () => read("/search/named", named(), "named-result"));
    on("dense-sparse-search", () => {
      const q = named();
      return read("/search/dense-sparse", { dense_name: q.name, dense: q.vector, k: q.k, ...(q.version ? { version: q.version } : {}), sparse_name: field("sparse-name"), sparse: sparseVector(get("sparse-query")), fusion: { candidates: 100, rrf_k: 60, vector_weight: 1, text_weight: 1 } }, "named-result");
    });
    on("geo-search", () => {
      const number = (name, min, max) => {
        const value = Number($(name).value);
        if (!$(name).value.trim() || !Number.isFinite(value) || value < min || value > max) throw new Error(`${name} must be ${min}..${max}`);
        return value;
      };
      return read("/search/geo", { ...base(), k: limit(), field: field("geo-field"), latitude: number("geo-latitude", -90, 90), longitude: number("geo-longitude", -180, 180), radius_meters: number("geo-radius", 0, Number.MAX_SAFE_INTEGER), sort: $("geo-sort").value }, "geo-result");
    });
    on("recommend-run", () => {
      const ids = (name) => $(name).value.split(",").map((v) => v.trim()).filter(Boolean).map(id);
      const examples = (name) => {
        const rows = get(name);
        if (!Array.isArray(rows) || rows.length > 1000) throw new Error("Expected up to 1000 example vectors");
        return rows.map((row) => vector(row, ctx.dimensions()));
      };
      const positive_ids = ids("recommend-positive"), positive_vectors = examples("recommend-vectors");
      if (!positive_ids.length && !positive_vectors.length) throw new Error("Recommend requires positive IDs or vectors");
      return read("/recommend", { positive_ids, negative_ids: ids("recommend-negative"), positive_vectors, negative_vectors: examples("recommend-negative-vectors"), k: limit(), metric: $("metric").value, filter: filter() }, "recommend-result");
    });
    on("facets-run", () => read("/facets", { field: field("facet-field"), filter: filter() }, "facets-result"));
    on("stats-run", () => read("/stats/numeric", { field: field("facet-field"), filter: filter() }, "stats-result"));
    on("language-run", () => {
      const query = $("language-query").value;
      if (!query.trim() || new TextEncoder().encode(query).length > 16384) throw new Error("Query must contain 1..16384 bytes");
      const vectors = get("language-vectors");
      if (!object(vectors)) throw new Error("Vector parameters must be a JSON object");
      Object.values(vectors).forEach((v) => vector(v, null));
      return read("/query", { query, vectors }, "language-result");
    });
    on("disk-build", async () => {
      if (!ctx.can("index")) throw new Error("HTTP 403: index operation required");
      const metric = $("metric").value;
      if (!["squared_l2", "cosine"].includes(metric)) throw new Error("Disk ANN supports squared L2 and cosine");
      if (!await ctx.confirm(`Build disk ANN for collection ${ctx.collection()}? The private sidecar is replaced; encrypted databases are unsupported.`)) return;
      text("disk-result", "Build pending; no automatic retry");
      display("disk-result", await ctx.api("POST", route("/index/disk-ann"), { metric, config: ctx.indexConfig() }, { data: false }));
    });
    on("disk-probe", () => read("/search/disk-ann", { vector: vector(get("query"), ctx.dimensions()), k: 1, ef_search: boundedInteger($("ef").value, 1, 100000, "Search ef") }, "disk-result"));
    const feedAllowed = () => {
      const caps = ctx.capabilities();
      return caps?.role === "admin" || caps?.role === "read_only" || (caps?.role === "scoped" && caps.scope.operations.includes("read") && caps.scope.collections.includes("*"));
    };
    async function loadFeed(cursor) {
      if (!feedAllowed()) throw new Error("HTTP 403: change feed requires read over all collections");
      nextCursor = null; hasMore = false;
      text("feed-result", ""); text("feed-status", "Loading page"); table("feed-table", []);
      const result = await ctx.api("GET", feedPath(cursor, $("feed-limit").value));
      nextCursor = result.next_cursor; hasMore = result.has_more === true;
      table("feed-table", feedRows(result.changes));
      display("feed-result", result.changes);
      text("feed-status", { head: result.head, next_cursor: nextCursor, has_more: hasMore });
    }
    on("feed-load", () => loadFeed($("feed-cursor").value.trim()));
    on("feed-next", () => {
      if (!hasMore || !nextCursor) throw new Error("No next page; load a cursor to refresh the feed");
      $("feed-cursor").value = nextCursor;
      return loadFeed(nextCursor);
    });
    $("feed-cursor").addEventListener("input", () => { nextCursor = null; hasMore = false; $("feed-next").disabled = true; });
    const admin = (feature) => {
      if (ctx.capabilities()?.role !== "admin") throw new Error("HTTP 403: bootstrap admin required");
      if (feature && !ctx.feature(feature)) throw new Error(`Server feature ${feature} is not enabled; inspect compatibility/configuration`);
    };
    const webhookId = () => {
      const value = $("webhook-id").value.trim();
      if (!/^[a-f0-9]{24}$/.test(value)) throw new Error("Expected a 24-character hexadecimal subscription ID");
      return value;
    };
    const list = async () => { admin("webhooks"); display("webhook-result", await ctx.api("GET", "/v1/admin/webhooks")); };
    on("webhook-list", list);
    on("webhook-status", async () => { admin("webhooks"); display("webhook-result", await ctx.api("GET", `/v1/admin/webhooks/${webhookId()}`)); });
    on("webhook-create", async () => {
      admin("webhooks");
      let body;
      try {
        body = webhookRequest({ url: $("webhook-url").value, collections: $("webhook-collections").value, graphs: $("webhook-graphs").value, events: $("webhook-events").value, secret: $("webhook-secret").value });
        if (!await ctx.confirm("Create this webhook subscription? Committed changes will be sent to the configured destination. Its signing secret cannot be recovered from the API.")) return;
        display("webhook-result", await ctx.api("POST", "/v1/admin/webhooks", body, { data: false, sensitive: true }));
      } finally { $("webhook-secret").value = ""; if (body) body.secret = ""; }
    });
    on("webhook-delete", async () => {
      admin("webhooks"); const value = webhookId();
      if (await ctx.confirm(`Delete webhook subscription ${value}? Delivery to this subscription stops.`)) {
        await ctx.api("DELETE", `/v1/admin/webhooks/${value}`, undefined, { data: false });
        await list();
      }
    });
    on("pitr-run", async () => {
      admin("backup_restore"); const body = pitrRequest($("pitr-backup").value.trim(), $("pitr-kind").value, $("pitr-target").value.trim());
      if (await ctx.confirm(`Restore backup ${body.backup} at ${$("pitr-kind").value} ${$("pitr-target").value} into a new copy? The live database is unchanged; no startup selection is made.`)) display("pitr-result", await ctx.api("POST", "/v1/admin/restores", body, { data: false }));
    });
    const results = ["group-result", "named-result", "geo-result", "recommend-result", "facets-result", "stats-result", "language-result", "disk-result"];
    return {
      dirty: () => draftDirty ? ["advanced retrieval draft"] : [],
      reset(all = false) {
        draftDirty = false;
        for (const name of results) text(name, "");
        for (const name of ["record-ttl", "named-version", "recommend-positive", "recommend-negative"]) $(name).value = "";
        $("named-query").value = "[]"; $("language-vectors").value = "{}";
        $("sparse-query").value = "[]";
        $("language-query").value = "SEARCH default NEAR $q K 10";
        $("recommend-vectors").value = $("recommend-negative-vectors").value = "[]";
        if (all) {
          nextCursor = null; hasMore = false;
          for (const name of ["feed-cursor", "webhook-url", "webhook-events", "webhook-collections", "webhook-graphs", "webhook-secret", "webhook-id", "pitr-backup", "pitr-target"]) $(name).value = "";
          for (const name of ["feed-result", "feed-status", "webhook-result", "pitr-result"]) text(name, "");
          table("feed-table", []);
        }
      },
      gate() {
        const caps = ctx.capabilities();
        document.querySelectorAll("[data-route]").forEach((element) => { element.disabled = !caps?.routes?.includes(element.dataset.route); });
        document.querySelectorAll("[data-feed]").forEach((element) => { element.disabled = !ctx.connected() || !ctx.feature("change_feed") || !feedAllowed(); });
        $("feed-next").disabled = !ctx.connected() || !ctx.feature("change_feed") || !feedAllowed() || !hasMore || !nextCursor;
        $("record-ttl").disabled = !ctx.feature("record_expiry") || !ctx.can("write");
        $("rerank-mode").disabled = !caps?.routes?.includes("/v1/collections/{collection}/query");
      },
    };
  }
  function studioTools(ctx, on, get) {
    let validated = null, stop = true, writing = false, reviewed = null;
    const clearCode = () => { text("studio-code", ""); };
    const invalidate = () => { validated = null; text("write-result", "Draft changed. Validate again before writing."); clearCode(); };
    $("write-documents").addEventListener("input", invalidate);
    $("write-stop").addEventListener("click", () => { stop = true; $("write-stop").disabled = true; });
    const requestIntro = $("requests").querySelector("p");
    if (requestIntro) requestIntro.textContent = "Latest request only. Authorization is hidden. Timing includes browser and network time. Requests are never retried automatically.";
    text("code-examples", "No example generated. Choose an allowlisted request and generate a text-only example; this does not send the request.");
    text("scenario-result", "No scenario run yet. Loading a file validates it locally; Run scenario sends the read-only requests.");
    on("write-validate", async () => {
      validated = null;
      $("write-progress").value = 0;
      const result = jsonDocuments($("write-documents").value, ctx.dimensions());
      text("write-result", { collection: ctx.collection(), documents: result.count, valid: result.valid, invalid: result.invalid, errors: result.errors, preview_ids: result.records.slice(0, 5).map((item) => item.id), note: result.invalid ? "Nothing can be written until all errors are fixed (first 20 shown)." : "Preview only; existing IDs will be replaced after confirmation." });
      if (!result.invalid) validated = { source: $("write-documents").value, owner: ctx.collection(), dimensions: ctx.dimensions(), records: result.records };
    });
    on("write-commit", async () => {
      if (!ctx.connected() || !ctx.can("write")) throw new Error("HTTP 403: connect with write access to the active collection");
      const draft = validated;
      if (!draft || draft.source !== $("write-documents").value || draft.owner !== ctx.collection() || draft.dimensions !== ctx.dimensions()) throw new Error("Validate this draft for the active collection first");
      if (!await ctx.confirm(`Upsert ${draft.records.length} records in collection ${draft.owner}? Existing vectors, payload, metadata and TTL will be replaced. Batches of 20 are atomic individually, NOT as a whole. Earlier batches remain committed on failure; no automatic retries.`)) return;
      if (draft.source !== $("write-documents").value || draft.owner !== ctx.collection()) throw new Error("Draft changed while confirming; validate again");
      validated = null; stop = false; writing = true;
      $("write-stop").disabled = false;
      $("write-progress").max = draft.records.length; $("write-progress").value = 0;
      let acknowledged = 0, batch = 0;
      const batches = chunk(draft.records.map((item) => ({ op: "put", ...item })), 20);
      try {
        for (const operations of batches) {
          if (stop) break;
          if (draft.owner !== ctx.collection() || !ctx.connected()) throw new Error("Collection or connection changed");
          batch++;
          text("write-result", `Collection ${draft.owner}: ${acknowledged}/${draft.records.length} acknowledged; sending batch ${batch}/${batches.length}.`);
          await ctx.api("POST", collectionPath(draft.owner, "/batches"), { operations });
          acknowledged += operations.length;
          $("write-progress").value = acknowledged;
        }
        text("write-result", `${acknowledged}/${draft.records.length} records acknowledged in ${draft.owner}. ${acknowledged === draft.records.length ? "Complete. Refresh Browse to inspect." : "Paused between batches. Earlier batches remain committed. Inspect before preparing a new draft of the remaining records."} Preview consumed; there is no automatic retry.`);
      } catch (error) {
        text("write-result", `${acknowledged}/${draft.records.length} records acknowledged in ${draft.owner}. Stopped at batch ${batch}; its outcome may be unknown. Earlier batches remain committed. Inspect server data before any manual retry. Preview consumed.`);
        throw error;
      } finally { writing = false; stop = true; $("write-stop").disabled = true; }
    });
    const draft = () => collectionDraft({ name: $("new-collection-name").value, dimensions: $("new-collection-dimensions").value, metric: $("new-collection-metric").value, indexes: $("new-collection-indexes").value, settings: $("new-collection-settings").value });
    const invalidateReview = () => { reviewed = null; text("collection-create-summary", "Step 1 changed. Review again before creating."); };
    for (const name of ["new-collection-name", "new-collection-dimensions", "new-collection-metric", "new-collection-indexes", "new-collection-settings"]) $(name).addEventListener("input", invalidateReview);
    on("collection-review", async () => { reviewed = null; const value = draft(); text("collection-create-summary", { step: "2 / Review (nothing written)", request: value, note: "Omitted settings use server defaults. Collection starts empty; active collection does not change." }); reviewed = JSON.stringify(value); });
    for (const name of ["studio-code-operation", "studio-code-language"]) $(name).addEventListener("input", clearCode);
    document.querySelectorAll("#search input, #search select, #search textarea, #editor input, #editor select, #editor textarea").forEach((element) => element.addEventListener("input", clearCode));
    on("studio-code-generate", async () => {
      clearCode();
      const operation = $("studio-code-operation").value;
      let body, recordId;
      if (operation === "search") body = ctx.searchBody();
      else if (operation === "upsert") {
        recordId = id($("id").value);
        const item = record({ id: recordId, vector: get("record-vector"), metadata: get("metadata"), payload_base64: $("payload").value }, ctx.dimensions());
        const { id: ignored, ...fields } = item;
        body = { ...fields, ...expiry($("record-ttl").value.trim()) };
      } else {
        const result = jsonDocuments($("write-documents").value, ctx.dimensions());
        if (result.invalid) throw new Error(result.errors.join("\n"));
        body = { operations: result.records.map((item) => ({ op: "put", ...item })) };
      }
      text("studio-code", codeSnippet({ kind: operation, collection: ctx.collection(), body, recordId, language: $("studio-code-language").value, legacy: operation === "search" ? !ctx.extended() : ctx.collection() === "default" }));
    });
    on("studio-code-copy", async () => {
      if (!$("studio-code").textContent) throw new Error("Generate code first");
      try { await navigator.clipboard.writeText($("studio-code").textContent); ctx.status("Code copied; the connected token was not included"); }
      catch { throw new Error("Clipboard unavailable. Select and copy the generated code manually."); }
    });
    const steps = [
      ["database", "Start here", "The overview shows your database at a glance, with shortcuts to common tasks. Use the sidebar to switch between Data, Server and Tools."],
      ["new-collection-name", "Choose a collection", "Pick the active collection in the sidebar, or create an empty one with its dimensions, metric and indexes. You review it before anything is created."],
      ["write-json", "Add records", "Paste JSON documents, check dimensions and IDs, review the preview, then confirm each batch. The tour never writes data."],
      ["search", "Search", "Paste a query vector and choose Top K. Start with exact search; use hybrid text retrieval only with configured text fields."],
      ["observability", "Observe", "Inspect health, queue pressure and latency. Observability values are live diagnostics, not a production monitoring service."],
    ];
    let step = -1;
    const unhighlight = () => document.querySelectorAll(".studio-highlight").forEach((element) => element.classList.remove("studio-highlight"));
    function tour() {
      $("studio-tour").hidden = false;
      if (step < 0) {
        text("tour-title", "Your first VLite workflow");
        text("tour-description", "Overview → collections → add records → search → observability. Five quick steps; the tour sends no requests and writes nothing.");
        text("tour-next", "Show me"); return;
      }
      const [target, title, description] = steps[step];
      text("tour-title", `${step + 1}/${steps.length} — ${title}`); text("tour-description", description);
      text("tour-next", step === steps.length - 1 ? "Finish tour" : "Next step");
      unhighlight();
      const element = revealAndScroll($(target) ? target : "diagnostics");
      element?.classList.add("studio-highlight");
      // Keep tour controls visible without a blocking modal or moving focus to a write control.
      $("studio-tour").classList.add("studio-tour-active");
    }
    function dismiss() {
      $("studio-tour").hidden = true; $("studio-tour").classList.remove("studio-tour-active"); unhighlight();
      try { localStorage.setItem("vlite-studio-tour", "dismissed"); } catch { /* Storage can be disabled; the tour still closes for this visit. */ }
    }
    $("tour-start").addEventListener("click", () => { step = -1; unhighlight(); tour(); });
    $("tour-next").addEventListener("click", () => { if (++step >= steps.length) dismiss(); else tour(); });
    $("tour-dismiss").addEventListener("click", dismiss);
    try { if (localStorage.getItem("vlite-studio-tour") !== "dismissed") tour(); } catch { tour(); }
    return {
      draft() { const value = draft(); if (reviewed !== JSON.stringify(value)) throw new Error("Review collection (step 2) before creating, especially after changing fields"); return value; },
      created() { reviewed = null; text("collection-create-summary", "Collection created. Select it in the sidebar before writing."); },
      clear() { validated = null; reviewed = null; stop = true; $("write-documents").value = ""; $("write-progress").value = 0; for (const name of ["write-result", "studio-code", "collection-create-summary"]) text(name, ""); },
      dirty() { return $("write-documents").value.trim() ? ["Write JSON draft / preview"] : []; },
      gate() { $("write-stop").disabled = !writing || stop; $("write-commit").disabled = !ctx.connected() || !ctx.can("write") || !validated; },
    };
  }
  function init(ctx) {
    let preview = [], previewCollection = null, sample = [], refreshing = false;
    // Resumable import state: prepared file chunks and the job being driven (never persisted).
    let prepared = null; // { collection, file, size, chunks, rows }
    let job = null; // { id, collection }
    let stopRequested = false;
    // One-time credential secret: closure memory only; never logged, inspected or stored.
    let secret = null;
    const on = (name, action) => {
      const element = $(name);
      if (!element) throw new Error(`console markup is missing control #${name}`);
      element.addEventListener("click", () => ctx.run(action));
    };
    const get = (name) => JSON.parse($(name).value);
    const advanced = advancedTools(ctx, on, get);
    const studio = studioTools(ctx, on, get);
    // Record fields only: newer servers add server-owned fields (e.g. `revision`) that are not writable and would make
    // exports non-importable and read-then-rewrite batches fail validation.
    const fetchRecord = async (value) => {
      const { id: recordId, vector: values, metadata: fields, payload_base64: payload } = await ctx.api("GET", ctx.path(`/vectors/${id(value)}`));
      return { id: recordId, vector: values, metadata: fields ?? {}, payload_base64: payload ?? "" };
    };
    // Index-changing and index-measuring routes: collection routes on extended servers (so the
    // collection's quantization/settings apply), legacy routes otherwise.
    // Settings-sensitive routes: the selected collection's own route on newer servers (never a silent legacy/default fallback).
    const indexPath = (suffix) => ctx.settingsPath(suffix);
    function metadataRow(name = "", entry = { type: "string", value: "" }) {
      if ($("metadata-fields").children.length >= 64) throw new Error("At most 64 metadata fields");
      const row = document.createElement("div"); row.className = "metadata-row";
      const field = document.createElement("input"); field.value = name;
      field.setAttribute("aria-label", "Metadata field name");
      const type = document.createElement("select"); type.setAttribute("aria-label", "Metadata field type");
      for (const value of ["string", "integer", "float", "boolean"]) type.append(new Option(value, value));
      type.value = entry.type;
      const value = document.createElement("input"); value.value = String(entry.value);
      value.setAttribute("aria-label", "Metadata field value");
      const remove = document.createElement("button"); remove.type = "button"; remove.textContent = "Remove field";
      remove.className = "secondary"; remove.addEventListener("click", () => row.remove());
      row.append(field, type, value, remove); $("metadata-fields").append(row);
    }
    on("metadata-load", async () => {
      const values = metadata(get("metadata"));
      $("metadata-fields").replaceChildren();
      for (const [name, entry] of Object.entries(values)) metadataRow(name, entry);
      ctx.status("Metadata draft loaded into visual fields; no database write");
    });
    on("metadata-add", async () => metadataRow());
    on("metadata-apply", async () => {
      const entries = [];
      for (const row of $("metadata-fields").children) {
        const [field, type, input] = row.children;
        let value = input.value;
        if (type.value === "float") {
          if (!value.trim()) throw new Error("Float fields require a number");
          value = Number(value);
        } else if (type.value === "boolean") {
          if (!["true", "false"].includes(value)) throw new Error("Boolean fields require true or false");
          value = value === "true";
        }
        if (entries.some(([name]) => name === field.value)) throw new Error("Duplicate metadata field names");
        entries.push([field.value, { type: type.value, value }]);
      }
      $("metadata").value = JSON.stringify(metadata(Object.fromEntries(entries)), null, 2);
      ctx.status("Visual fields applied to JSON draft only; use Put / replace to save");
    });
    on("query-save", async () => {
      if (!ctx.extended() && $("rerank-mode").value) throw new Error("Legacy query files cannot store reranking; disable it first");
      const base = { k: Number($("k").value), mode: $("mode").value, metric: $("metric").value, ef_search: Number($("ef").value), filter: get("filter"), config: ctx.indexConfig() };
      let value;
      if (ctx.extended()) {
        const query = $("query").value.trim();
        value = savedQueryV2({ version: 2, name: $("query-name").value, collection: ctx.collection(), query: { ...base,
          ...rerank($("rerank-mode").value, $("rerank-candidates").value, $("mmr-lambda").value, base.k, base.mode),
          vector: query ? get("query") : null, text: $("search-text").value, explain: $("explain").checked,
          quantization: $("quantization").value, rerank_multiplier: Number($("rerank-multiplier").value), vector_leg: $("vector-leg").value,
          fusion: { rrf_k: Number($("rrf-k").value), vector_weight: Number($("vector-weight").value), text_weight: Number($("text-weight").value), candidates: Number($("fusion-candidates").value) },
        } }, ctx.dimensions());
      } else value = savedQuery({ version: 1, name: $("query-name").value, query: { vector: get("query"), ...base } }, ctx.dimensions());
      download("vlite-query.json", value);
      text("query-workspace-status", `Downloaded "${value.name}"${value.collection ? ` for collection ${value.collection}` : ""}. Query data may be private; no credential included.`);
    });
    on("query-load", async () => {
      const source = JSON.parse(await readFile($("query-file").files[0]));
      // Version 2 files are bound to a collection and are refused for any other collection.
      const value = object(source) && source.version === 2 ? savedQueryV2(source, ctx.dimensions(), ctx.collection()) : savedQuery(source, ctx.dimensions());
      const q = value.query;
      if (["auto", "hybrid"].includes(q.mode) && $("mode").querySelector(`option[value="${q.mode}"]`).disabled) throw new Error(`This server does not advertise ${q.mode} search; nothing was loaded`);
      $("query-name").value = value.name; $("query").value = q.vector === null ? "" : JSON.stringify(q.vector);
      $("mode").value = q.mode;
      $("rerank-mode").value = q.rerank?.mode ?? "";
      $("rerank-candidates").value = q.rerank?.candidates ?? 50;
      $("mmr-lambda").value = q.rerank?.lambda ?? 0.5;
      $("metric").value = q.metric; $("k").value = q.k; $("ef").value = q.ef_search;
      $("filter").value = JSON.stringify(q.filter, null, 2);
      $("hnsw-m").value = q.config.m; $("hnsw-ef").value = q.config.ef_construction; $("hnsw-seed").value = q.config.seed;
      if (value.version === 2) {
        $("search-text").value = q.text; $("explain").checked = q.explain && ctx.feature("search_explain");
        $("quantization").value = q.quantization; $("rerank-multiplier").value = q.rerank_multiplier; $("vector-leg").value = q.vector_leg;
        $("rrf-k").value = q.fusion.rrf_k; $("vector-weight").value = q.fusion.vector_weight; $("text-weight").value = q.fusion.text_weight; $("fusion-candidates").value = q.fusion.candidates;
      }
      $("mode").dispatchEvent(new Event("change"));
      text("query-workspace-status", `Loaded "${value.name}" into the form only${value.collection ? ` (collection ${value.collection})` : ""}. Select Search to replay; approximate mode may build an index.`);
    });
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
    async function commit(items, owner = ctx.collection()) {
      for (let start = 0; start < items.length; start += 20) {
        if (owner !== ctx.collection()) throw new Error(`Collection changed; stopped after ${start}/${items.length} operations (those remain committed in ${owner})`);
        text("import-result", `${start}/${items.length} operations acknowledged in collection ${owner}; sending next atomic batch. Do not retry an unknown outcome.`);
        await ctx.api("POST", collectionPath(owner, "/batches"), { operations: items.slice(start, start + 20) });
      }
      text("import-result", `${items.length}/${items.length} operations committed to collection ${owner}. Refresh the browse page.`);
    }
    function validatePreview(items) {
      if (ctx.dimensions() === null) throw new Error("Sign in before validating records");
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
      if (!size || size > 4096) throw new Error("The demo needs a collection with 1..4096 dimensions. Sign in and select one first.");
      preview = validatePreview([1, 2, 3].map((n) => ({ id: String(n), vector: Array.from({ length: size }, (_, j) => j === 0 ? n / 3 : j === 1 ? 1 - n / 3 : 0), metadata: { category: { type: "string", value: "demo" } } })));
      previewCollection = ctx.collection();
      text("import-result", { instructions: "Preview only. Commit replaces IDs 1,2,3 after confirmation; then browse and Find similar to saved ID.", collection: previewCollection, records: preview });
    });
    on("import-preview", async () => {
      preview = [];
      text("import-result", "Validating import; no writes have been sent");
      const file = $("import-file").files[0];
      const source = await readFile(file);
      preview = validatePreview(file.name.toLowerCase().endsWith(".csv") ? csvParse(source) : JSON.parse(source));
      previewCollection = ctx.collection();
      text("import-result", { validated_records: preview.length, collection: previewCollection, preview });
    });
    on("import-commit", async () => {
      if (!preview.length) throw new Error("Validate or prepare a preview first");
      if (previewCollection !== ctx.collection()) throw new Error("The preview was validated for another collection; validate it again");
      const items = preview, owner = previewCollection;
      if (!await ctx.confirm(`Commit ${items.length} records to collection ${owner} in batches of 20? Existing IDs will be replaced. Earlier batches remain committed if a later batch fails.`)) return;
      preview = [];
      await commit(items.map((item) => ({ op: "put", ...item })), owner);
    });
    for (const format of ["json", "csv"]) on(`export-page-${format}`, async () => {
      const items = await records(pageIds());
      download(`vlite-page.${format}`, format === "csv" ? csvExport(items) : items, format === "csv" ? "text/csv" : "application/json");
    });
    on("export-hits", async () => {
      const fields = [...$("hits").querySelectorAll("thead th")].map((cell) => cell.dataset.field || cell.textContent.toLowerCase());
      const hits = [...$("hits").querySelectorAll("tbody tr")].map((row) => Object.fromEntries(fields.map((field, index) => {
        const value = row.cells[index].textContent;
        return [field, field === "id" ? value : value === "--" ? null : Number(value)];
      })));
      if (!hits.length) throw new Error("Run a search first");
      download("vlite-search.json", { collection: ctx.lastSearch()?.collection ?? ctx.collection(), metric: $("hits").caption.textContent, hits });
    });
    on("batch-delete", async () => {
      const ids = selectedIds(), owner = ctx.collection();
      if (!await ctx.confirm(`Delete ${ids.length} selected records from collection ${owner}? This cannot be undone. Batches of 20 are atomic; the whole selection is not.`)) return;
      text("import-result", `Deleting ${ids.length} selected records from collection ${owner}; nothing acknowledged yet`);
      await commit(ids.map((value) => ({ op: "delete", id: value })), owner);
    });
    on("batch-replace", async () => {
      const ids = selectedIds(), replacement = metadata(get("batch-metadata")), owner = ctx.collection();
      if (!await ctx.confirm(`Replace entire metadata maps on ${ids.length} records in collection ${owner}? Records are read then rewritten, not compare-and-swap; use a dedicated test database without concurrent writers.`)) return;
      text("import-result", `Reading ${ids.length} selected records from collection ${owner}; nothing written yet`);
      const items = await records(ids);
      await commit(items.map((item) => ({ op: "put", ...record({ ...item, metadata: replacement }, ctx.dimensions()) })), owner);
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
    });
    on("filter-clear", async () => { $("filter").value = "[]"; });
    on("find-similar", async () => {
      const item = await fetchRecord($("id").value);
      $("query").value = JSON.stringify(item.vector);
      $("filter").value = "[]"; $("mode").value = "exact"; $("mode").dispatchEvent(new Event("change"));
      await ctx.search();
      revealAndScroll("search");
    });
    on("compare-search", async () => {
      if ($("metric").value === "inner_product") throw new Error("ANN comparison supports squared L2 and cosine only");
      const filter = get("filter");
      if (!Array.isArray(filter)) throw new Error("Filter must be an AND array");
      if (filter.length && ctx.capabilities()?.features?.filtered_ann !== true) throw new Error("This server does not advertise filtered ANN; use exact search or upgrade the server");
      text("search-comparison", "Running exact then ready-only ANN; the two reads are not a snapshot");
      const body = { vector: vector(get("query"), ctx.dimensions()), k: Number($("k").value), metric: $("metric").value, config: ctx.indexConfig(), filter };
      const started = performance.now();
      const exact = await ctx.api("POST", indexPath("/search"), { ...body, mode: "exact" });
      const exactMs = performance.now() - started, annStart = performance.now();
      const ann = await ctx.api("POST", indexPath("/search"), { ...body, mode: "approximate_ready", ef_search: Number($("ef").value), ...ctx.annOptions() });
      const common = exact.hits.filter((hit) => ann.hits.some((other) => other.id === hit.id)).length;
      text("search-comparison", { exact_ms: exactMs, ann_ms: performance.now() - annStart, common_ids: common, exact_count: exact.hits.length, ann_count: ann.hits.length, overlap_fraction: exact.hits.length ? common / exact.hits.length : null, note: "Same filter on both queries, live reads not a snapshot. Browser timing; overlap with exact top-k on this query, not a general recall benchmark. ANN can underfill; no exact fallback. Null overlap means no exact matches.", exact, ann });
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
      const page = await ctx.api("GET", ctx.path("/vectors?limit=32"));
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
        circle.addEventListener("click", () => ctx.run(() => ctx.inspect(ctx.path(`/vectors/${point.id}`))));
        circle.addEventListener("keydown", (event) => {
          if (event.key === "Enter" || event.key === " ") { event.preventDefault(); ctx.run(() => ctx.inspect(ctx.path(`/vectors/${point.id}`))); }
        });
        const title = document.createElementNS(svg.namespaceURI, "title"); title.textContent = `Record ${point.id}`; circle.append(title); svg.append(circle);
      });
      $("projection").append(svg);
      const caption = document.createElement("p"); caption.textContent = "PCA sample IDs (keyboard-accessible inspection):"; $("projection").append(caption);
      points.forEach((point) => {
        const button = document.createElement("button"); button.type = "button"; button.textContent = point.id;
        button.addEventListener("click", () => ctx.run(() => ctx.inspect(ctx.path(`/vectors/${point.id}`)))); $("projection").append(button);
      });
    });
    function requestSpec() {
      const method = $("request-method").value, path = $("request-path").value;
      if (!requestAllowed(method, path)) throw new Error("Playground path/method is not a supported route; no external origins, queries or credentials allowed");
      return { method, path, ...(method === "GET" || method === "DELETE" ? {} : { body: get("request-body") }) };
    }
    on("send-request", async () => {
      const spec = requestSpec();
      if (spec.method !== "GET" && !isSearch(spec.path) && !await ctx.confirm(`Send ${spec.method} ${spec.path}? This can durably change data; no automatic retry.`)) return;
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
        if (isSearch(step.path)) {
          if (!object(step.body) || Object.keys(step.body).some((key) => !["vector", "k", "mode", "metric", "filter", "ef_search"].includes(key))) throw new Error("Invalid scenario search body");
          vector(step.body.vector, ctx.dimensions());
        } else if (!new RegExp(`^/v1${SCOPED}/vectors/${ID}$`).test(step.path) || step.body !== undefined) throw new Error("Only record GET and search steps are allowed");
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
        const result = await ctx.api(isSearch(step.path) ? "POST" : "GET", step.path, step.body);
        const actual = isSearch(step.path) ? result.hits.map((hit) => hit.id) : [result.id];
        results.push({ path: step.path, expected: step.expected_ids, actual, passed: JSON.stringify(actual) === JSON.stringify(step.expected_ids) });
        text("scenario-result", { name: value.name, results });
      }
    });
    async function health() {
      const caps = ctx.capabilities();
      const metrics = caps?.role !== "scoped" || ctx.can("diagnostics") ? await ctx.api("GET", "/metrics") : "Unavailable: scoped credential lacks the diagnostics operation";
      const index = await ctx.api("GET", indexPath("/index/hnsw"));
      text("health-result", { observed_at: new Date().toISOString(), metrics, index, queue_note: "Request admission is bounded; this is a snapshot, not live telemetry" });
    }
    on("refresh-health", health);
    function admin(feature) {
      const caps = ctx.capabilities();
      if (!caps || caps.role !== "admin") throw new Error("HTTP 403: admin credential required");
      if (feature && !caps.features?.[feature]) throw new Error(`Server feature ${feature} is not enabled; inspect compatibility/configuration`);
    }
    // Admin, or a scoped credential holding `operation` for the active collection.
    function permit(feature, operation) {
      if (!ctx.can(operation)) throw new Error(`HTTP 403: this credential lacks the ${operation} operation for collection ${ctx.collection()}`);
      if (feature && !ctx.feature(feature)) throw new Error(`Server feature ${feature} is not enabled; inspect compatibility/configuration`);
    }
    on("status-rebuild", async () => text("rebuild-result", await ctx.api("GET", indexPath("/index/hnsw/rebuild"))));
    on("start-rebuild", async () => {
      permit("background_rebuild", "index");
      if (!await ctx.confirm(`Start a bounded background rebuild for collection ${ctx.collection()}? Snapshot capture briefly owns the database; worker memory is additional to the active graph.`)) return;
      text("rebuild-result", await ctx.api("POST", indexPath("/index/hnsw/rebuild"), { config: ctx.indexConfig(), metric: $("metric").value, ...ctx.annOptions(), limits: {
        max_records: Number($("hnsw-max-records").value), max_snapshot_vector_bytes: Number($("hnsw-max-bytes").value),
      } }));
    });
    on("poll-rebuild", async () => {
      permit("background_rebuild", "index");
      text("rebuild-result", await ctx.api("POST", indexPath("/index/hnsw/rebuild/poll"), {}));
    });
    on("cancel-rebuild", async () => {
      permit("background_rebuild", "index");
      if (!await ctx.confirm("Request cooperative cancellation? The current active graph is preserved; poll to reap the worker.")) return;
      text("rebuild-result", await ctx.api("POST", indexPath("/index/hnsw/rebuild/cancel"), {}));
    });
    // The policy PUT exists only on the legacy (default) route; on newer servers its thresholds are global and
    // each collection route takes its index identity from that collection's settings.
    function maintenanceScope() {
      const named = ctx.collection() !== "default";
      if (!ctx.collection()) return text("maintenance-scope", "No vector collection selected; maintenance applies once a collection exists.");
      text("maintenance-scope", !ctx.extended()
        ? "Maintenance policy applies to the single (default) database."
        : named
          ? `Collection ${ctx.collection()}: read and tick use this collection's route; its metric/HNSW/quantization identity comes from its stored settings (edit them under Collections). The policy can only be replaced from the default collection.`
          : "Collection default: read and tick use the default collection route (its stored settings). Replacing the policy uses the legacy route: its thresholds apply to every collection; its config/metric apply only to legacy /v1/index routes.");
    }
    on("read-maintenance", async () => {
      const owner = ctx.collection();
      const result = await ctx.api("GET", indexPath("/index/hnsw/maintenance"));
      if (owner !== ctx.collection()) throw new Error("Collection changed while reading; result discarded");
      $("maintenance-policy").value = JSON.stringify(result.policy, null, 2);
      text("maintenance-result", { collection: owner, ...result });
    });
    on("save-maintenance", async () => {
      admin("maintenance");
      if (ctx.collection() !== "default") throw new Error(`The maintenance policy can only be replaced while the default collection is selected; collection ${ctx.collection()} uses its stored settings`);
      const policy = get("maintenance-policy");
      if (!await ctx.confirm(`Replace the entire process-local maintenance policy via the legacy route (default collection)? Omitted fields use defaults, not current values. No timer is started.${ctx.extended() ? " Thresholds apply to every collection; config/metric apply only to legacy /v1/index routes, while collection routes use each collection's stored settings." : ""}`)) return;
      if (ctx.collection() !== "default") throw new Error("Collection changed before the write; nothing sent");
      text("maintenance-result", await ctx.api("PUT", "/v1/index/hnsw/maintenance", policy));
    });
    on("tick-maintenance", async () => {
      permit("maintenance", "index");
      if (!await ctx.confirm(`Run one maintenance tick for collection ${ctx.collection()}? It may publish a finished graph or start a background rebuild.`)) return;
      text("maintenance-result", await ctx.api("POST", indexPath("/index/hnsw/maintenance/tick"), {}));
    });
    on("token-status", async () => {
      admin(); text("token-result", await ctx.api("GET", "/v1/admin/tokens"));
    });
    on("token-reload", async () => {
      admin("tokens_reload");
      if (!await ctx.confirm("Reload only configured secret files? Invalid files leave credentials unchanged. You are signed out afterwards; sign in again, with the new token if it changed.")) return;
      const result = await ctx.api("POST", "/v1/admin/tokens/reload", {});
      ctx.disconnect();
      text("token-result", result);
      ctx.status("Configured credentials reloaded. Sign in again with a current token.");
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
    on("list-backups", async () => { admin("backup_restore"); await listBackups(); });
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
    // ---- EPIC-015 surfaces ----
    const when =  (ms) => ms === null || ms === undefined ? "--" : new Date(ms).toISOString();
    function requireExtended() {
      if (!ctx.extended()) throw new Error("This server predates collections, settings and index diagnostics; upgrade the server");
    }
    function formatNote(version) {
      text("collections-format", version >= 4
        ? `Database format v${version}: named collections and resumable imports are available.`
        : `Database format v${version}: only the default collection exists. Named collections and resumable imports need format v4. Upgrade is offline and non-destructive: stop the server, run "vlite-maintain upgrade SOURCE DEST" to write a NEW v4 file (the source is never modified), then start the server on DEST. The console never upgrades automatically.`);
    }
    function renderCollections(items) {
      table("collections-table", items.map((item) => [item.name, item.dimensions, item.live_records, item.settings?.metric, item.settings?.quantization,
        item.settings_persisted === false ? "memory only (lost on restart)" : "persisted"]));
    }
    on("collections-refresh", async () => {
      requireExtended();
      const listed = await ctx.refreshCollections();
      formatNote(listed.format_version);
      renderCollections(listed.items);
    });
    on("collection-create", async () => {
      requireExtended();
      if (!ctx.feature("collections")) throw new Error("HTTP 409: requires_format_v4; collections need a format v4 database file");
      const body = studio.draft();
      const { name, dimensions } = body;
      if (!await ctx.confirm(`Create empty collection ${name} with ${dimensions} dimensions${body.settings ? " and the given settings" : " and default settings"}? The active collection does not change.`)) return;
      const created = await ctx.api("POST", "/v1/collections", body, { data: false });
      const listed = await ctx.refreshCollections();
      renderCollections(listed.items);
      text("collection-result", { created });
      studio.created();
      $("new-collection-name").value = "";
    });
    on("collection-drop", async () => {
      requireExtended();
      const name = collectionName($("drop-collection-name").value.trim());
      if (name === "default") throw new Error("The default collection cannot be dropped");
      if (!ctx.collections().some((item) => item.name === name)) throw new Error("No such collection in the current list; refresh it");
      if (!await ctx.confirm(`Drop collection ${name}? The server only drops EMPTY collections (delete its records first). Credentials scoped to it stop working, even if a collection with the same name is created later.${name === ctx.collection() ? " It is the active collection; the console will switch away." : ""}`)) return;
      const result = await ctx.api("DELETE", `/v1/collections/${name}`, undefined, { data: false });
      const listed = await ctx.refreshCollections();
      renderCollections(listed.items);
      text("collection-result", { dropped: name, result });
      $("drop-collection-name").value = "";
    });
    // Without a collection there is no record route to suggest, so the scenario starts empty.
    function defaultScenario() {
      const steps = ctx.collection() ? [{ path: ctx.path("/vectors/1"), expected_ids: ["1"] }] : [];
      return JSON.stringify({ name: "get-record", steps }, null, 2);
    }
    on("settings-load", async () => {
      requireExtended();
      const info = await ctx.api("GET", ctx.settingsPath(""));
      $("collection-settings").value = JSON.stringify(info.settings, null, 2);
      text("collection-result", { collection: info.name, settings_persisted: info.settings_persisted, text_index: info.text_index ?? null });
    });
    on("settings-save", async () => {
      requireExtended();
      const owner = ctx.collection();
      const change = settings(get("collection-settings"));
      if (!Object.keys(change).length) throw new Error("No settings to change");
      if (!await ctx.confirm(`Apply settings to collection ${owner}? Omitted fields keep their current values. Changing metric, HNSW or quantization makes an existing index stale until rebuilt; text fields and metadata indexes are rebuilt by the server.`)) return;
      const result = await ctx.api("PUT", ctx.settingsPath("/settings"), change, { data: false });
      text("collection-result", { collection: result.collection, persisted: result.persisted,
        note: result.persisted ? "Settings are durable." : "Applied in memory only (format v3 default collection); they reset when the server restarts." });
      $("collection-settings").value = JSON.stringify(result.collection.settings, null, 2);
      ctx.applySettings(result.collection);
    });
    // Index memory and quality.
    on("index-memory", async () => {
      requireExtended();
      const result = await ctx.api("GET", indexPath("/index/hnsw"));
      const rows = memoryRows(result);
      table("index-memory-table", rows.length ? [
        ["graph nodes", result.graph_nodes], ["stale nodes", result.stale_nodes], ["quantization", result.quantization],
        ...rows,
      ] : [["no index built", "--"]]);
    });
    on("quality-run", async () => {
      requireExtended();
      permit(null, "index");
      const body = {
        metric: $("metric").value, config: ctx.indexConfig(), ...ctx.annOptions(),
        samples: boundedInteger($("quality-samples").value, 1, 32, "Samples"), k: boundedInteger($("quality-k").value, 1, 100, "k"),
        ef_search: boundedInteger($("ef").value, 1, 100000, "ef"), seed: id($("hnsw-seed").value),
        max_distance_evaluations: boundedInteger($("quality-budget").value, 1, 1000000000, "Budget"),
      };
      text("quality-result", "Measuring; read-only, may take a while");
      const result = await ctx.api("POST", indexPath("/index/hnsw/quality"), body, { data: false });
      text("quality-result", { collection: ctx.collection(), ...result,
        summary: result.recall_at_k === null ? `Not measured: ${result.skipped_reason === "budget" ? "distance evaluation budget too small" : "no live records to sample"}` : `recall@${body.k} = ${result.recall_at_k} over ${result.samples_used} sampled queries (estimate, not a guarantee)` });
    });
    // Resumable import.
    function jobText(value) { text("job-result", value); }
    function progress(done, total) { $("job-progress").max = Math.max(1, total); $("job-progress").value = done; }
    on("job-prepare", async () => {
      prepared = null;
      const file = $("job-file").files[0];
      if (!file) throw new Error("Choose an import file first");
      const size = boundedInteger($("job-chunk-size").value, 1, 4096, "Rows per chunk");
      const rows = importRows(file.name, await readFile(file));
      const dims = ctx.dimensions();
      const wrong = rows.findIndex((row) => row.vector.length !== dims);
      if (wrong >= 0) throw new Error(`Row ${wrong + 1}: ${rows[wrong].vector.length} dimensions; collection ${ctx.collection()} has ${dims}`);
      prepared = { collection: ctx.collection(), file: file.name, size, chunks: chunk(rows, size), rows: rows.length };
      progress(0, prepared.chunks.length);
      jobText({ collection: prepared.collection, file: prepared.file, rows: prepared.rows, chunks: prepared.chunks.length, rows_per_chunk: size, note: "Validated locally; nothing sent. Keep this file and chunk size to resume later." });
    });
    function ensurePrepared() {
      if (!prepared) throw new Error("Validate / split the import file first (the same file and chunk size used for the job)");
      if (prepared.collection !== ctx.collection()) throw new Error("The file was prepared for another collection; prepare it again");
    }
    async function sendChunks(from) {
      const total = prepared.chunks.length, policy = $("job-policy").value, invalid = [];
      stopRequested = false;
      for (let n = from; n <= total; n += 1) {
        if (stopRequested) { jobText({ job: job.id, collection: job.collection, stopped_before_chunk: n, note: "Stopped. Committed chunks are retained; Resume continues from the durable checkpoint." }); return; }
        if (job.collection !== ctx.collection()) throw new Error("Collection changed; import paused (committed chunks are retained)");
        let report;
        try {
          report = await ctx.api("PUT", `/v1/imports/${job.id}/chunks/${n}`, { operations: prepared.chunks[n - 1], policy });
        } catch (error) {
          if (error.code === "import_rows_invalid" && error.body) {
            text("job-invalid", { chunk: n, invalid: error.body.invalid });
            jobText({ job: job.id, rejected_chunk: n, note: `Chunk ${n} was rejected and nothing from it was written. Chunks 1..${n - 1} remain committed. Fix the rows (or choose skip_invalid) and Resume with the same chunk size.` });
          } else jobText({ job: job.id, chunk: n, note: `Outcome of chunk ${n} is unknown or failed (${error.message}). Nothing is retried automatically; use Resume, which reads the durable checkpoint first.` });
          throw error;
        }
        if (report.invalid?.length) invalid.push(...report.invalid.slice(0, 100 - invalid.length).map((row) => ({ chunk: n, ...row })));
        progress(n, total);
        jobText({ job: job.id, collection: job.collection, checkpoint_chunk: n, of: total, outcome: report.outcome, rows_committed: report.rows_committed, rows_skipped: report.rows_skipped });
        if (invalid.length) text("job-invalid", { first_100_skipped_rows: invalid });
      }
      jobText({ job: job.id, collection: job.collection, all_chunks_sent: total, note: "All chunks are committed. Finish the job to close it." });
    }
    on("job-start", async () => {
      ensurePrepared();
      if (!ctx.feature("imports")) throw new Error("This server/database does not support resumable imports (format v4 required)");
      const owner = prepared.collection;
      if (!await ctx.confirm(`Import ${prepared.rows} records from ${prepared.file} into collection ${owner} as ${prepared.chunks.length} chunk(s) of up to ${prepared.size}? Existing IDs are replaced. Each chunk commits atomically; committed chunks are kept if the import stops or is aborted.`)) return;
      const created = await ctx.api("POST", `/v1/collections/${owner}/imports`);
      job = { id: created.job, collection: created.collection };
      $("job-id").value = created.job;
      text("job-invalid", "");
      await sendChunks(1);
    });
    async function readJob() {
      const value = $("job-id").value.trim();
      id(value);
      return ctx.api("GET", `/v1/imports/${value}`);
    }
    on("job-status", async () => {
      const status = await readJob();
      jobText({ ...status, note: status.collection_dropped ? "The target collection was dropped." : status.durable ? "Durable checkpoint." : "Not yet durable: a job with no committed chunk does not survive a restart." });
    });
    on("job-resume", async () => {
      ensurePrepared();
      const status = await readJob();
      if (status.collection !== ctx.collection() || status.collection !== prepared.collection) throw new Error(`Job ${status.job} belongs to collection ${status.collection}; select it and prepare the same file`);
      if (status.collection_dropped) throw new Error("The job's collection was dropped; it cannot be resumed");
      const plan = resumePlan(status, prepared.chunks.length);
      if (plan.action === "closed" || plan.action === "mismatch") { jobText({ ...status, note: plan.message }); return; }
      if (!await ctx.confirm(`Resume job ${status.job} into collection ${status.collection}? ${plan.message}`)) return;
      job = { id: status.job, collection: status.collection };
      progress(plan.last, prepared.chunks.length);
      if (plan.verify) {
        const check = await ctx.api("PUT", `/v1/imports/${job.id}/chunks/${plan.verify}`, { operations: prepared.chunks[plan.verify - 1], policy: $("job-policy").value });
        if (check.outcome !== "already_committed") throw new Error(`Chunk ${plan.verify} replay reported ${check.outcome}; stopping. Inspect the job status before continuing.`);
      }
      await sendChunks(plan.next);
    });
    $("job-stop").addEventListener("click", () => { stopRequested = true; ctx.status("Stop requested; the chunk in flight completes, then the import pauses"); });
    for (const [name, verb] of [["job-finish", "finish"], ["job-abort", "abort"]]) on(name, async () => {
      const status = await readJob();
      if (status.collection !== ctx.collection()) throw new Error(`Job ${status.job} belongs to collection ${status.collection}; select it first`);
      if (!await ctx.confirm(verb === "finish"
        ? `Finish job ${status.job} in collection ${status.collection}? ${status.last_chunk} committed chunk(s) (${status.rows_committed} rows) stay; no further chunks are accepted.`
        : `Abort job ${status.job} in collection ${status.collection}? The ${status.last_chunk} chunk(s) (${status.rows_committed} rows) ALREADY COMMITTED ARE RETAINED, not rolled back. No further chunks are accepted.`)) return;
      const result = await ctx.api("POST", `/v1/imports/${status.job}/${verb}`, {});
      if (job?.id === status.job) job = null;
      jobText(result);
    });
    // Schedulers.
    on("auto-maintenance-read", async () => {
      admin("auto_maintenance");
      const result = await ctx.api("GET", "/v1/admin/maintenance/auto");
      $("auto-maintenance").value = JSON.stringify(autoMaintenanceForm(result.config || {}), null, 2);
      text("auto-maintenance-result", result);
    });
    on("auto-maintenance-save", async () => {
      admin("auto_maintenance");
      const policy = autoMaintenance(get("auto-maintenance"));
      const changes = Object.entries(policy).map(([key, value]) => `${key}=${JSON.stringify(value)}`).join(", ");
      if (!await ctx.confirm(`${policy.enabled === true ? "ENABLE automatic maintenance for all collections: it may start background rebuilds (at most one at a time). " : policy.enabled === false ? "Disable automatic maintenance. " : ""}Merge ${changes} into the automatic maintenance policy (omitted fields keep their current values)? Memory-only: restart restores the environment configuration.`)) return;
      text("auto-maintenance-result", await ctx.api("PUT", "/v1/admin/maintenance/auto", policy, { data: false }));
    });
    on("auto-maintenance-tick", async () => {
      admin("auto_maintenance");
      if (!await ctx.confirm("Run exactly one bounded automatic-maintenance tick now? It may act on one collection (round-robin) and may start one background rebuild. It runs even if the timer is disabled.")) return;
      text("auto-maintenance-result", await ctx.api("POST", "/v1/admin/maintenance/auto/tick", undefined, { data: false }));
    });
    on("backup-schedule-run", async () => {
      admin("backup_schedule");
      if (!await ctx.confirm("Run one scheduled backup cycle now? It writes a backup to the admin directory (disk and I/O) and, per the current schedule, may run a restore drill and apply retention (retention deletes only when enabled).")) return;
      text("backup-schedule-result", await ctx.api("POST", "/v1/admin/backups/schedule/run", undefined, { data: false }));
    });
    let scheduleConfig = null, credentialDefault = null;
    on("backup-schedule-read", async () => {
      admin("backup_schedule");
      const result = await ctx.api("GET", "/v1/admin/backups/schedule");
      scheduleConfig = result.config;
      $("backup-schedule").value = JSON.stringify(result.config, null, 2);
      text("backup-schedule-result", result);
    });
    on("backup-schedule-save", async () => {
      admin("backup_schedule");
      const changes = backupSchedule(get("backup-schedule"));
      if (!scheduleConfig) throw new Error("Read the current schedule first so the confirmation can describe the merged result");
      const schedule = { ...scheduleConfig, ...changes };
      const parts = [schedule.enabled ? `Back up every ${schedule.interval_secs} s` : "Scheduled backups OFF",
        schedule.retention_enabled ? `RETENTION ON: generated backups beyond the newest ${schedule.retain_last}${schedule.retain_days ? ` and older than ${schedule.retain_days} days` : ""} will be DELETED (the newest backup, newest verified backup, next-startup target, active database and non-generated files are never deleted)` : "retention off (nothing is deleted)",
        schedule.drill_every ? `restore drill every ${schedule.drill_every} backup(s)` : "no drills"];
      if (!await ctx.confirm(`Update the backup schedule? Result: ${parts.join("; ")}. Memory-only: restart restores the environment configuration.`)) return;
      const result = await ctx.api("PUT", "/v1/admin/backups/schedule", changes, { data: false });
      if (result?.config) scheduleConfig = result.config;
      text("backup-schedule-result", result);
    });
    for (const [name, verb] of [["backup-verify", "verify"], ["backup-drill", "drill"]]) on(name, async () => {
      admin("backup_verify_drill");
      const backup = $("backup-name").value;
      if (!backup) throw new Error("List snapshots and select a generated backup first");
      const confirmation = verb === "drill"
        ? `Run a restore drill for ${backup}? It writes a temporary scratch copy in the admin directory (disk and I/O), validates and queries it, then deletes it. The active database is not touched.`
        : `Verify ${backup}? This reads and validates the full snapshot and updates its verification manifest in the admin directory. The active database is not touched.`;
      if (!await ctx.confirm(confirmation)) return;
      try {
        text("backup-result", await ctx.api("POST", `/v1/admin/backups/${backup}/${verb}`, {}, { data: false }));
      } catch (error) {
        if (error.body) text("backup-result", { backup, result: `${verb} failed`, response: redact(error.body) });
        throw error;
      }
    });
    // Diagnostics.
    on("slow-queries", async () => {
      const result = await ctx.api("GET", "/v1/admin/diagnostics/slow-queries");
      text("slow-summary", `${result.items.length} of at most ${result.capacity} retained; threshold ${result.threshold_ms} ms. Process-local; resets on restart.`);
      table("slow-table", result.items.map((item) => [when(item.unix_ms), item.route, item.collection, item.mode, item.plan_strategy, item.k, item.duration_ms, item.result_count]));
    });
    on("prometheus", async () => {
      const body = await ctx.api("GET", "/metrics?format=prometheus", undefined, { text: true });
      text("prometheus-result", body.length > 65536 ? `${body.slice(0, 65536)}\n... (${body.length - 65536} more characters not shown)` : body);
    });
    // Scoped credentials: the token is held only in `secret` and rendered masked.
    function showSecret(value, credential) {
      secret = value;
      text("cred-secret", value ? `New secret for ${credential.id}${credential.name ? ` (${credential.name})` : ""}: ${maskSecret(value)}. Scope: ${credentialScope(credential)}; operations [${credential.operations.join(", ")}]. Shown once: copy it now; it cannot be retrieved again.` : "No new secret.");
    }
    function credentialRows(items) {
      table("cred-table", items.map((item) => {
        const actions = document.createElement("span");
        const state = item.state ?? item.status;
        if (state === "active") {
          for (const [label, verb, className] of [["Rotate", "rotate", "secondary"], ["Revoke", "revoke", "danger"]]) {
            const button = document.createElement("button");
            button.type = "button"; button.className = className; button.textContent = `${label} ${item.id}`;
            button.dataset.admin = ""; button.dataset.feature = "scoped_credentials";
            button.addEventListener("click", () => ctx.run(() => credentialAction(item, verb)));
            actions.append(button);
          }
        }
        const scope = credentialScope(item);
        return [item.id, item.name, scope, item.operations.join(", "), state, when(item.expires_unix_ms), actions];
      }));
    }
    async function listCredentials() {
      const result = await ctx.api("GET", "/v1/admin/credentials", undefined, { data: false });
      credentialRows(result.items);
      text("cred-result", `${result.items.length} of ${result.capacity} credential slots used. Secrets and hashes are never returned.`);
    }
    // 503 credentials_commit_indeterminate: the change IS in effect (no token returned) but may not survive a crash.
    // Never present it as "no change": say so, then re-list so the admin can revoke or re-issue.
    async function credentialMutation(action, label) {
      try {
        return await action();
      } catch (error) {
        if (error.code === "credentials_commit_indeterminate") {
          try { await listCredentials(); } catch { /* the original error is reported below */ }
          text("cred-result", `${label}: the change IS in effect but its durability is not confirmed and no secret was returned. Review the list below, then revoke or re-issue as needed.`);
        }
        throw error;
      }
    }
    async function credentialAction(item, verb) {
      admin("scoped_credentials");
      if (verb === "rotate") {
        const grace = graceSeconds($("cred-grace").value);
        if (!await ctx.confirm(`Rotate credential ${item.id}? Scope stays unchanged: ${credentialScope(item)}; operations [${item.operations.join(", ")}]. A new secret is shown once. The old secret ${grace ? `keeps working for ${grace} seconds, then stops` : "stops working immediately"}.`)) return;
        showSecret(null);
        const result = await credentialMutation(() => ctx.api("POST", `/v1/admin/credentials/${item.id}/rotate`, { grace_seconds: grace }, { sensitive: true, data: false }), `Rotate ${item.id}`);
        showSecret(result.token, result.credential);
      } else {
        if (!await ctx.confirm(`Revoke credential ${item.id}${item.name ? ` (${item.name})` : ""}? Requests using it are refused from now on. This cannot be undone.`)) return;
        showSecret(null);
        await credentialMutation(() => ctx.api("POST", `/v1/admin/credentials/${item.id}/revoke`, {}, { data: false }), `Revoke ${item.id}`);
      }
      await listCredentials();
    }
    on("cred-create", async () => {
      admin("scoped_credentials");
      const body = credentialRequest({ name: $("cred-name").value.trim(), collections: $("cred-collections").value, graphs: $("cred-graphs").value,
        operations: [...document.querySelectorAll('input[name="cred-op"]:checked')].map((input) => input.value), days: $("cred-days").value,
        tenant_id: $("cred-tenant")?.value, requests_per_second: $("cred-rpm")?.value,
        max_vectors: $("cred-max-vectors")?.value, max_vectors_per_collection: $("cred-max-vectors-per-collection")?.value });
      if (body.tenant_id != null) admin("tenants");
      if ("requests_per_second" in body) admin("credential_rate_limits");
      if ("max_vectors" in body || "max_vectors_per_collection" in body) admin("credential_vector_quotas");
      if (!await ctx.confirm(`Create a credential with ${credentialScope(body)}; operations [${body.operations.join(", ")}], expiring ${when(body.expires_unix_ms)}? The secret is shown once.`)) return;
      showSecret(null);
      const result = await credentialMutation(() => ctx.api("POST", "/v1/admin/credentials", body, { sensitive: true, data: false }), "Create");
      showSecret(result.token, result.credential);
      $("cred-name").value = "";
      await listCredentials();
    });
    on("cred-list", async () => { admin("scoped_credentials"); await listCredentials(); });
    on("cred-copy", async () => {
      if (!secret) throw new Error("No secret to copy");
      if (!navigator.clipboard) throw new Error("Clipboard unavailable (requires HTTPS or localhost); the secret cannot be displayed in full");
      await navigator.clipboard.writeText(secret);
      ctx.status("Secret copied to the clipboard. Store it in a secret manager, then Clear.");
    });
    on("cred-clear", async () => { showSecret(null); ctx.status("Secret cleared from this page"); });
    on("persistent-audit", async () => {
      admin("persistent_audit");
      const limit = boundedInteger($("audit-limit").value, 1, 1000, "Entries");
      const result = await ctx.api("GET", `/v1/admin/audit/persistent?limit=${limit}`);
      text("persistent-audit-result", redact(result));
    });
    const timer = setInterval(() => {
      if (!$("auto-health").checked || document.hidden || ctx.busy() || !ctx.connected() || refreshing) return;
      refreshing = true;
      ctx.run(health).finally(() => { refreshing = false; });
    }, 10000);
    window.addEventListener("pagehide", () => clearInterval(timer), { once: true });
    const systemTheme = window.matchMedia("(prefers-color-scheme: dark)");
    function applyAppearance() {
      const chosen = $("theme").value;
      document.documentElement.dataset.theme = chosen === "system" ? (systemTheme.matches ? "dark" : "light") : chosen;
      document.documentElement.dataset.density = $("density").value;
    }
    function appearance() {
      applyAppearance();
      try {
        if ($("remember-preferences").checked) {
          localStorage.setItem("vlite-appearance", JSON.stringify({ theme: $("theme").value, density: $("density").value }));
          if ($("theme").value === "system") localStorage.removeItem("vlite-theme");
          else localStorage.setItem("vlite-theme", $("theme").value);
        } else {
          localStorage.removeItem("vlite-appearance");
          localStorage.removeItem("vlite-theme");
        }
      } catch { ctx.status("Appearance changed for this visit; browser storage is unavailable"); }
    }
    for (const key of ["theme", "density", "remember-preferences"]) $(key).addEventListener("change", appearance);
    const systemChanged = () => { if ($("theme").value === "system") applyAppearance(); };
    systemTheme.addEventListener("change", systemChanged);
    window.addEventListener("pagehide", () => systemTheme.removeEventListener("change", systemChanged), { once: true });
    try {
      const saved = localStorage.getItem("vlite-appearance");
      if (saved) {
        const value = JSON.parse(saved);
        if (["light", "dark", "system"].includes(value.theme) && ["compact", "comfortable"].includes(value.density)) {
          $("theme").value = value.theme; $("density").value = value.density; $("remember-preferences").checked = true;
        }
      }
      const shared = localStorage.getItem("vlite-theme");
      if (["light", "dark"].includes(shared)) {
        $("theme").value = shared;
        $("remember-preferences").checked = true;
      }
    } catch (error) { ctx.status(`Appearance preferences could not be loaded: ${error.message}`, true); }
    window.addEventListener("storage", (event) => {
      if (event.key !== "vlite-theme") return;
      $("theme").value = ["light", "dark"].includes(event.newValue) ? event.newValue : "system";
      applyAppearance();
    });
    applyAppearance();
    return {
      clear() {
        advanced.reset(true);
        studio.clear();
        $("metadata-fields").replaceChildren();
        $("query-name").value = "my-query"; $("query-file").value = "";
        text("query-workspace-status", "");
        preview = []; previewCollection = null; sample = []; $("auto-health").checked = false; $("projection").replaceChildren();
        prepared = null; job = null; stopRequested = true; showSecret(null); scheduleConfig = null;
        for (const name of ["import-result", "sample-analysis", "record-comparison", "search-comparison", "scenario-result", "health-result", "code-examples", "payload-preview", "vector-slice-result", "vector-summary", "compatibility", "rebuild-result", "maintenance-result", "maintenance-scope", "token-result", "audit-result", "backup-result",
          "collection-result", "quality-result", "job-result", "job-invalid", "auto-maintenance-result", "backup-schedule-result", "slow-summary", "prometheus-result", "cred-result", "persistent-audit-result"]) text(name, "");
        for (const name of ["batch-ids", "request-body", "scenario", "filter-value", "import-file", "scenario-file", "job-file", "job-id", "new-collection-name", "drop-collection-name", "cred-name"]) $(name).value = "";
        for (const name of ["cred-graphs", "cred-tenant", "cred-rpm", "cred-max-vectors", "cred-max-vectors-per-collection"]) if ($(name)) $(name).value = "";
        $("batch-metadata").value = "{}"; $("collection-settings").value = "{}"; $("new-collection-settings").value = "{}";
        for (const name of ["collections-table", "index-memory-table", "slow-table", "cred-table"]) $(name).querySelector("tbody").replaceChildren();
        progress(0, 1);
        text("collections-format", "Sign in to check collection support.");
        $("backup-name").replaceChildren(); $("restore-name").replaceChildren();
      },
      connected(caps) {
        const f = caps.features || {};
        const flags = ["collections", "imports", "per_record_etags", "search_auto", "search_hybrid", "search_explain", "quantization", "auto_maintenance", "backup_schedule", "backup_verify_drill", "slow_query_log", "prometheus_metrics", "scoped_credentials", "persistent_audit"];
        const available = flags.filter((flag) => f[flag] === true), missing = flags.filter((flag) => f[flag] !== true);
        text("admin-note", `Role: ${caps.role}${caps.scope ? ` (collections ${caps.scope.collections.join(", ")}; operations ${caps.scope.operations.join(", ")}; expires ${when(caps.scope.expires_unix_ms)})` : ""}; API v${caps.api_version}${caps.format_version !== undefined ? `; database format v${caps.format_version}` : ""}. Backup/restore ${f.backup_restore ? "enabled" : "disabled: configure VLITE_ADMIN_DIR"}. Token-file reload ${f.tokens_reload ? "available" : "unavailable: use configured files or restart"}. Available: ${available.join(", ") || "none of the newer features"}. Not available on this server/configuration: ${missing.join(", ") || "none"}.`);
        if (caps.format_version !== undefined) formatNote(caps.format_version);
        else text("collections-format", "This server predates collections; only the default database is available.");
        if (ctx.extended()) renderCollections(ctx.collections());
        maintenanceScope();
        $("cred-collections").value = credentialDefault = ctx.collection();
        $("scenario").value = defaultScenario();
        $("request-body").value = "{}";
      },
      loaded(item) {
        $("metadata-fields").replaceChildren();
        for (const [name, entry] of Object.entries(item.metadata)) metadataRow(name, entry);
        text("vector-summary", `${item.vector.length} components. Inspect a 32-component slice below; full vector remains editable.`);
        try {
          const bytes = Uint8Array.from(atob(item.payload_base64), (char) => char.charCodeAt(0));
          const decoded = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
          try { text("payload-preview", JSON.parse(decoded)); } catch { text("payload-preview", decoded || "(empty payload)"); }
        } catch { text("payload-preview", "Binary payload: use the base64 field. No UTF-8 preview available."); }
      },
      // Collection-bound pending work that a collection switch would discard.
      dirty() {
        const lost = [...advanced.dirty(), ...studio.dirty()];
        if (preview.length) lost.push(`validated import preview (${preview.length} records)`);
        if (prepared) lost.push(`prepared resumable import file ${prepared.file} (${prepared.chunks.length} chunks)`);
        if (job) lost.push(`progress view of import job ${job.id} (the job stays open on the server)`);
        if ($("batch-ids").value.trim()) lost.push("batch ID selection");
        if ($("metadata-fields").children.length) lost.push("visual metadata fields");
        return lost;
      },
      collectionChanged(info) {
        advanced.reset();
        studio.clear();
        preview = []; previewCollection = null; sample = []; prepared = null; stopRequested = true;
        // Follow the selection only while the scope field still holds the untouched default.
        if (info?.name && $("cred-collections").value === credentialDefault) $("cred-collections").value = credentialDefault = info.name;
        const previousJob = job; job = null;
        $("projection").replaceChildren(); $("metadata-fields").replaceChildren();
        for (const name of ["import-result", "sample-analysis", "record-comparison", "search-comparison", "scenario-result", "payload-preview", "vector-slice-result", "vector-summary", "rebuild-result", "maintenance-result", "quality-result", "job-invalid"]) text(name, "");
        for (const name of ["batch-ids", "import-file", "job-file"]) $(name).value = "";
        maintenanceScope();
        $("index-memory-table").querySelector("tbody").replaceChildren();
        $("collection-settings").value = info?.settings ? JSON.stringify(info.settings, null, 2) : "{}";
        progress(0, 1);
        text("job-result", previousJob ? `Import job ${previousJob.id} (collection ${previousJob.collection}) remains open on the server; select that collection and Resume to continue.` : "");
        $("scenario").value = defaultScenario();
      },
      // Recompute state gates in both directions; authorization gates run afterward.
      gate() {
        advanced.gate();
        studio.gate();
        document.querySelectorAll("[data-extended]").forEach((element) => { element.disabled = !ctx.connected() || !ctx.extended(); });
        $("cred-copy").disabled = $("cred-clear").disabled = !ctx.connected() || secret === null;
        $("job-stop").disabled = true;
        $("save-maintenance").disabled = !ctx.connected() || ctx.collection() !== "default";
        const caps = ctx.capabilities();
        const diagnosticsAllowed = ctx.connected() && (caps?.role !== "scoped" || ctx.can("diagnostics"));
        $("prometheus").disabled = !diagnosticsAllowed || !ctx.feature("prometheus_metrics");
        $("refresh-health").disabled = !diagnosticsAllowed;
      },
    };
  }
  window.VLiteTools = { init, utils: {
    jsonDocuments, collectionDraft, codeSnippet, responseSummary,
    id, vector, metadata, record, savedQuery, csvParse, csvExport, distances, analyze, pca,
    collectionName, collectionPath, settings, scoreDirection, redact, maskSecret, OPERATIONS, credentialRequest, credentialScope, graceSeconds,
    expiry, rerank, sparseVector, webhookRequest, WEBHOOK_EVENTS, feedRows, pitrRequest, feedPath,
    boundedInteger, importRows, chunk, resumePlan, savedQueryV2, autoMaintenance, autoMaintenanceForm, backupSchedule, memoryRows, requestAllowed, isSearch,
  } };
})();
