"use strict";
// Read-only property graph explorer (FOLLOWUP-002). Self-initialising against
// <section id="graph">; does nothing when the section is absent. Graph controls stay hidden
// (and the a[data-workspace="graph"] link is hidden) unless GET /v1/graphs answers 200, or 403
// on a graph-enabled build, which shows a "no graph access" notice instead.
// Requests use window.VLiteGraphConnection.request(method, path, body) -> {status, value} when
// the dashboard provides it (token sessions), otherwise a same-origin cookie request.
// Pure helpers are exported for Node tests.
(() => {
  const U64_MAX = 18446744073709551615n;
  const SAFE_MAX = 9007199254740991;
  const OPERATIONS = {
    "get-node": { path: "nodes/get", title: "Get node by ID or key" },
    "get-edge": { path: "edges/get", title: "Get edge by ID or key" },
    "find-nodes": { path: "nodes/find", title: "Find nodes by label or property" },
    neighbors: { path: "neighbors", title: "Neighbors" },
    traverse: { path: "traverse", title: "Bounded traversal" },
    "shortest-path": { path: "shortest-path", title: "Shortest path" },
  };
  const LIMITS = ["max_hops", "max_visited_nodes", "max_edges_examined", "max_edges_per_node", "deadline_ms",
    "max_output_nodes", "max_output_edges", "max_output_paths"];
  const DIRECTIONS = ["out", "in", "both"];
  const VALUE_TYPES = ["string", "integer", "float", "boolean"];
  const STATS_ROUTE = "/v1/admin/graph/stats";
  const VISIBLE = new Set(["available", "denied", "error"]);
  const SVG_NS = "http://www.w3.org/2000/svg";

  const object = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
  const text = (value) => (typeof value === "string" ? value : value === undefined || value === null ? "" : String(value));

  function graphPath(name, suffix) {
    const graph = text(name);
    if (!graph) throw new Error("Choose a graph first; no request sent");
    return `/v1/graphs/${encodeURIComponent(graph)}${suffix ? `/${suffix}` : ""}`;
  }

  // IDs are u64: sent as canonical decimal strings so large values are not rounded.
  function selector(kind, value, what = "Node") {
    const raw = text(value);
    if (kind === "id") {
      const id = raw.trim();
      if (!/^(0|[1-9][0-9]{0,19})$/.test(id) || BigInt(id) > U64_MAX) throw new Error(`${what} ID must be a canonical unsigned 64-bit integer; no request sent`);
      return { id };
    }
    if (kind !== "key") throw new Error(`${what} selector must be an ID or a key; no request sent`);
    if (raw === "") throw new Error(`${what} key is required; no request sent`);
    return { key: raw };
  }

  function selectorList(kind, value) {
    const lines = text(value).split(/\r?\n/).filter((line) => line.trim() !== "");
    if (!lines.length) throw new Error("Traverse needs at least one start node (one per line); no request sent");
    return lines.map((line) => selector(kind, kind === "id" ? line.trim() : line, "Start node"));
  }

  function list(value) {
    return text(value).split(",").map((item) => item.trim()).filter(Boolean);
  }

  function conditions(value, what = "Conditions") {
    const raw = text(value).trim();
    if (!raw) return [];
    let parsed;
    try { parsed = JSON.parse(raw); } catch (error) { throw new Error(`${what} must be a JSON array: ${error.message}; no request sent`); }
    if (!Array.isArray(parsed) || !parsed.every((item) => object(item) && typeof item.op === "string")) {
      throw new Error(`${what} must be a JSON array of {"op": "eq" | "range" | "georadius", ...} objects; no request sent`);
    }
    return parsed;
  }

  function typedValue(type, value) {
    const raw = text(value);
    switch (type) {
      case "string": return { type, value: raw };
      case "integer":
        if (!/^-?(0|[1-9][0-9]*)$/.test(raw.trim())) throw new Error("Integer value must be a whole number; no request sent");
        return { type, value: raw.trim() };
      case "float": {
        const number = Number(raw.trim());
        if (raw.trim() === "" || !Number.isFinite(number)) throw new Error("Float value must be a finite number; no request sent");
        return { type, value: number };
      }
      case "boolean":
        if (raw.trim() !== "true" && raw.trim() !== "false") throw new Error("Boolean value must be true or false; no request sent");
        return { type, value: raw.trim() === "true" };
      default: throw new Error("Unknown value type; no request sent");
    }
  }

  function equality(field, type, value) {
    const name = text(field).trim();
    if (!name) return [];
    return [{ op: "eq", field: name, value: typedValue(type, value) }];
  }

  function count(value, what) {
    const raw = text(value).trim();
    if (raw === "") return undefined;
    if (!/^(0|[1-9][0-9]*)$/.test(raw) || Number(raw) > SAFE_MAX) throw new Error(`${what} must be a non-negative integer up to ${SAFE_MAX}; no request sent`);
    return Number(raw);
  }

  // Unset limits keep the server defaults; values above the server ceiling are rejected there.
  function limits(values) {
    const out = {};
    for (const name of LIMITS) {
      const value = count(object(values) ? values[name] : undefined, name);
      if (value !== undefined) out[name] = value;
    }
    return out;
  }

  function projection(mode, fields) {
    if (mode === undefined || mode === "" || mode === "all") return "all";
    if (mode === "none") return "none";
    if (mode !== "only") throw new Error("Projection must be all, none or only; no request sent");
    const names = list(fields);
    if (!names.length) throw new Error("List at least one property name to project; no request sent");
    return { only: names };
  }

  function cursor(value) {
    const raw = text(value).trim();
    if (!raw) return undefined;
    if (!/^([0-9a-f]{2})+$/.test(raw)) throw new Error("Cursor must be the lowercase hex string a previous page returned; no request sent");
    return raw;
  }

  function direction(value) {
    const raw = text(value) || "out";
    if (!DIRECTIONS.includes(raw)) throw new Error("Direction must be out, in or both; no request sent");
    return raw;
  }

  function filter(labels, nodeConditions, what) {
    const out = {};
    const names = list(labels);
    const items = conditions(nodeConditions, what);
    if (names.length) out.labels = names;
    if (items.length) out.conditions = items;
    return Object.keys(out).length ? out : undefined;
  }

  // Builds the request body for one read operation from plain form values.
  function body(operation, form = {}) {
    const out = {};
    const kind = form.selectorKind || "key";
    const types = list(form.types);
    switch (operation) {
      case "get-node": out.node = selector(kind, form.selector, "Node"); break;
      case "get-edge": out.edge = selector(kind, form.selector, "Edge"); break;
      case "find-nodes": {
        if (text(form.label).trim()) out.label = text(form.label).trim();
        const items = [...conditions(form.conditions, "Node conditions"), ...equality(form.eqField, form.eqType || "string", form.eqValue)];
        if (items.length) out.conditions = items;
        break;
      }
      case "neighbors": {
        out.node = selector(kind, form.selector, "Node");
        out.direction = direction(form.direction);
        if (types.length) out.types = types;
        const edges = conditions(form.edgeConditions, "Edge conditions");
        if (edges.length) out.edge_conditions = edges;
        break;
      }
      case "traverse": {
        out.start = selectorList(kind, form.start);
        out.direction = direction(form.direction);
        if (types.length) out.types = types;
        const expand = filter(form.expandLabels, form.expandConditions, "Expand conditions");
        const keep = filter(form.returnLabels, form.conditions, "Node conditions");
        const edges = conditions(form.edgeConditions, "Edge conditions");
        if (expand) out.expand_filter = expand;
        if (keep) out.return_filter = keep;
        if (edges.length) out.edge_filter = edges;
        if (form.includePaths) out.include_paths = true;
        break;
      }
      case "shortest-path":
        out.source = selector(kind, form.selector, "Source node");
        out.destination = selector(kind, form.destination, "Destination node");
        out.direction = direction(form.direction);
        if (types.length) out.types = types;
        break;
      default: throw new Error("Unknown graph operation; no request sent");
    }
    if (operation === "find-nodes" || operation === "neighbors") {
      const limit = count(form.limit, "Page size");
      if (limit !== undefined) out.limit = limit;
      const next = cursor(form.cursor);
      if (next) out.cursor = next;
    }
    const bounds = limits(form.limits);
    if (Object.keys(bounds).length) out.limits = bounds;
    const shape = projection(form.projection, form.projectionFields);
    if (shape !== "all") out.projection = shape;
    return out;
  }

  function request(operation, graph, form, explain = false) {
    const payload = body(operation, form);
    if (explain) return { method: "POST", path: graphPath(graph, "explain"), body: { operation, ...payload } };
    return { method: "POST", path: graphPath(graph, OPERATIONS[operation].path), body: payload };
  }

  function errorCode(value) {
    return object(value) && object(value.error) ? text(value.error.code) : "";
  }

  function errorText(status, value) {
    if (object(value) && object(value.error)) {
      const prefix = status ? `HTTP ${status} ` : "";
      return `${prefix}${text(value.error.code) || "error"}: ${text(value.error.message)}`.trim();
    }
    if (typeof value === "string" && value) return `HTTP ${status}: ${value}`;
    return status ? `HTTP ${status}` : "No response from the server";
  }

  function openapiHasGraphs(document) {
    return object(document) && object(document.paths) && Object.keys(document.paths).some((path) => path === "/v1/graphs" || path.startsWith("/v1/graphs/"));
  }

  // 409 graphs_not_enabled = vector-only file; 404 = build without the graph feature;
  // 403 is ambiguous (unmatched routes are admin-only), so the OpenAPI document decides.
  function availability(status, code, hasGraphPaths) {
    if (status === 200) return "available";
    if (status === 409 && code === "graphs_not_enabled") return "disabled";
    if (status === 401) return "signed-out";
    if (status === 403) return hasGraphPaths ? "denied" : "absent";
    if (status === 404 || status === 405) return "absent";
    if (status === 0) return "offline";
    return "error";
  }

  function truncationText(items) {
    if (!Array.isArray(items) || !items.length) return "none";
    return items.map((item) => {
      if (typeof item === "string") return item;
      if (object(item) && object(item.degree_limit)) {
        const ids = Array.isArray(item.degree_limit.node_ids) ? item.degree_limit.node_ids : [];
        return `degree_limit (nodes ${ids.join(", ")})`;
      }
      return JSON.stringify(item);
    }).join("; ");
  }

  function indexText(items) {
    if (!Array.isArray(items) || !items.length) return "none";
    return items.map((item) => {
      if (typeof item === "string") return item;
      if (object(item) && object(item.adjacency_range_scan)) {
        const scan = item.adjacency_range_scan;
        return `adjacency_range_scan (${text(scan.direction)}${scan.typed ? ", typed" : ""})`;
      }
      return JSON.stringify(item);
    }).join(", ");
  }

  function valueText(value) {
    if (object(value) && "value" in value) return text(value.value);
    if (typeof value === "string") return value;
    return JSON.stringify(value);
  }

  function propertiesText(properties) {
    if (!object(properties)) return "";
    return Object.keys(properties).map((name) => `${name}=${valueText(properties[name])}`).join("; ");
  }

  // Flattens every read response shape into nodes/edges/paths tables.
  function normalize(value) {
    const out = { nodes: [], edges: [], paths: [], outcome: null, missing: false };
    if (!object(value)) return out;
    if ("node" in value) { if (object(value.node)) out.nodes.push(value.node); else out.missing = true; }
    if ("edge" in value) { if (object(value.edge)) out.edges.push(value.edge); else out.missing = true; }
    if (Array.isArray(value.nodes)) {
      for (const item of value.nodes) {
        if (object(item) && object(item.node)) out.nodes.push({ ...item.node, depth: item.depth });
        else if (object(item)) out.nodes.push(item);
      }
    }
    if (Array.isArray(value.edges)) out.edges.push(...value.edges.filter(object));
    if (Array.isArray(value.paths)) out.paths.push(...value.paths.filter(object));
    if (object(value.shortest_path)) {
      const found = value.shortest_path;
      out.outcome = text(found.outcome);
      if (Array.isArray(found.nodes)) out.nodes.push(...found.nodes.filter(object));
      if (Array.isArray(found.edges)) out.edges.push(...found.edges.filter(object));
      if (object(found.path)) out.paths.push(found.path);
      if (out.outcome === "exhausted") out.outcomeDetail = truncationText(found.truncation);
    }
    return out;
  }

  function summary(value) {
    if (!object(value)) return "";
    const stats = object(value.stats) ? value.stats : {};
    const parts = [
      value.complete === false ? "Incomplete (bounded by limits)" : "Complete",
      `truncation: ${truncationText(value.truncation)}`,
      `visited ${text(stats.nodes_visited) || 0}, scanned ${text(stats.nodes_scanned) || 0}, edges examined ${text(stats.edges_examined) || 0}`,
      `${text(stats.elapsed_ms) || 0} ms`,
      `indexes: ${indexText(stats.indexes)}`,
    ];
    if (value.view_sequence !== undefined) parts.push(`view ${text(value.view_sequence)}`);
    if (value.cursor) parts.push("more results: use Next page");
    return parts.join(" · ");
  }

  function explainRows(value) {
    const plan = object(value) && object(value.explain) ? value.explain : {};
    const query = object(plan.query) ? { ...plan.query } : {};
    const operation = text(query.operation);
    delete query.operation;
    const bounds = object(plan.limits) ? Object.keys(plan.limits).map((name) => `${name}=${text(plan.limits[name])}`).join(", ") : "";
    const shape = typeof plan.projection === "string" ? plan.projection : JSON.stringify(plan.projection ?? "all");
    return [["Operation", operation], ["Query", JSON.stringify(query)], ["Limits", bounds], ["Projection", shape], ["Indexes", indexText(plan.indexes)]];
  }

  function statsCards(value) {
    const guard = (item) => (object(item) ? `${text(item.p50_us) || 0} / ${text(item.p99_us) || 0} µs` : "n/a");
    const requests = object(value) && object(value.requests) ? value.requests : {};
    return [
      ["Read guard p50 / p99", guard(value && value.read_guard)],
      ["Write guard p50 / p99", guard(value && value.write_guard)],
      ["Read requests", text(requests.read) || "0"],
      ["Write requests", text(requests.write) || "0"],
    ];
  }

  function truncationRows(value) {
    const counts = object(value) && object(value.truncations) ? value.truncations : {};
    return Object.keys(counts).sort().map((reason) => [reason, text(counts[reason])]);
  }

  // Dependency-free layout: traversal depths become columns, anything else a circle.
  function layout(result, options = {}) {
    const width = options.width || 720;
    const height = options.height || 360;
    const cap = options.cap || 40;
    const pad = 40;
    const byId = new Map();
    const add = (id, key, labels, depth) => {
      if (id === undefined || id === null) return;
      const name = String(id);
      const known = byId.get(name);
      if (!known) { byId.set(name, { id: name, key: key ?? null, labels: Array.isArray(labels) ? labels : [], depth: depth ?? null }); return; }
      if (known.key === null && key !== undefined && key !== null) known.key = key;
      if (known.depth === null && depth !== undefined && depth !== null) known.depth = depth;
    };
    for (const node of result.nodes || []) add(node.id, node.key, node.labels, node.depth);
    for (const edge of result.edges || []) { add(edge.source, edge.source_key); add(edge.target, edge.target_key); }
    const all = [...byId.values()];
    const shown = all.slice(0, cap);
    const placed = new Map();
    if (shown.some((node) => node.depth !== null)) {
      const deepest = Math.max(0, ...shown.map((node) => (node.depth === null ? 0 : Number(node.depth))));
      const columns = new Map();
      for (const node of shown) {
        const column = node.depth === null ? deepest + 1 : Number(node.depth);
        if (!columns.has(column)) columns.set(column, []);
        columns.get(column).push(node);
      }
      const order = [...columns.keys()].sort((a, b) => a - b);
      order.forEach((column, index) => {
        const x = order.length === 1 ? width / 2 : pad + (index * (width - 2 * pad)) / (order.length - 1);
        const members = columns.get(column);
        members.forEach((node, row) => placed.set(node.id, { x, y: members.length === 1 ? height / 2 : pad + (row * (height - 2 * pad)) / (members.length - 1) }));
      });
    } else if (shown.length === 1) {
      placed.set(shown[0].id, { x: width / 2, y: height / 2 });
    } else {
      const radius = Math.min(width, height) / 2 - pad;
      shown.forEach((node, index) => {
        const angle = -Math.PI / 2 + (index * 2 * Math.PI) / shown.length;
        placed.set(node.id, { x: width / 2 + radius * Math.cos(angle), y: height / 2 + radius * Math.sin(angle) });
      });
    }
    const round = (value) => Math.round(value * 10) / 10;
    const nodes = shown.map((node) => ({ ...node, x: round(placed.get(node.id).x), y: round(placed.get(node.id).y), text: text(node.key ?? node.id) }));
    const edges = (result.edges || []).filter((edge) => placed.has(String(edge.source)) && placed.has(String(edge.target)))
      .map((edge) => ({ id: text(edge.id), label: text(edge.relationship), source: String(edge.source), target: String(edge.target) }));
    return { width, height, nodes, edges, omitted: all.length - shown.length };
  }

  // ---------------------------------------------------------------- DOM

  function h(doc, tag, props = {}, ...children) {
    const element = doc.createElement(tag);
    for (const [name, value] of Object.entries(props)) {
      if (value === undefined || value === null || value === false) continue;
      if (name === "text") element.textContent = value;
      else if (name === "class") element.className = value;
      else if (name === "hidden" || name === "disabled" || name === "checked") element[name] = true;
      else element.setAttribute(name, value === true ? "" : String(value));
    }
    for (const child of children.flat()) if (child) element.append(child);
    return element;
  }

  function field(doc, ops, label, control) {
    return h(doc, "label", { "data-graph-ops": ops }, h(doc, "span", { text: label }), control);
  }

  function options(doc, id, values, labels = values) {
    return h(doc, "select", { id }, values.map((value, index) => h(doc, "option", { value, text: labels[index] })));
  }

  function table(doc, id, headings) {
    return h(doc, "div", { class: "table-scroll", hidden: true, "data-graph-table": id },
      h(doc, "table", { id }, h(doc, "thead", {}, h(doc, "tr", {}, headings.map((heading) => h(doc, "th", { scope: "col", text: heading })))), h(doc, "tbody")));
  }

  function markup(doc) {
    const operationNames = Object.keys(OPERATIONS);
    const limitFields = LIMITS.map((name) => field(doc, "", name, h(doc, "input", { id: `graph-limit-${name}`, inputmode: "numeric", placeholder: "Server default" })));
    return [
      h(doc, "span", { class: "eyebrow", text: "PROPERTY GRAPH" }),
      h(doc, "h2", { id: "graph-title", text: "Graphs" }),
      h(doc, "p", { text: "Explore the property graphs stored in this database file. This view is read-only: change graphs with the REST batch route, vlite-graph-cli or MCP." }),
      h(doc, "p", { id: "graph-note", "aria-live": "polite", text: "Checking whether this server and file have graphs enabled…" }),
      h(doc, "button", { id: "graph-refresh", type: "button", class: "secondary", text: "Check graph access again" }),
      h(doc, "div", { id: "graph-controls", hidden: true },
        h(doc, "div", { class: "tool-group" },
          h(doc, "div", { class: "section-heading" }, h(doc, "h3", { text: "Graphs in this file" }), h(doc, "button", { id: "graph-list", type: "button", class: "secondary", text: "List graphs" })),
          table(doc, "graph-table", ["Name", "Nodes", "Edges", "Use"]),
          h(doc, "label", {}, "Graph", h(doc, "select", { id: "graph-name" }))),
        h(doc, "div", { class: "tool-group" },
          h(doc, "h3", { text: "Query" }),
          h(doc, "label", {}, "Operation", options(doc, "graph-operation", operationNames, operationNames.map((name) => OPERATIONS[name].title))),
          field(doc, "get-node get-edge neighbors traverse shortest-path", "Select nodes and edges by", options(doc, "graph-selector-kind", ["key", "id"], ["Key", "ID (unsigned 64-bit)"])),
          h(doc, "label", { "data-graph-ops": "get-node get-edge neighbors shortest-path" }, h(doc, "span", { id: "graph-selector-label", text: "Node" }), h(doc, "input", { id: "graph-selector", spellcheck: "false", autocomplete: "off" })),
          field(doc, "shortest-path", "Destination node", h(doc, "input", { id: "graph-destination", spellcheck: "false", autocomplete: "off" })),
          field(doc, "traverse", "Start nodes (one per line)", h(doc, "textarea", { id: "graph-start", rows: "3", spellcheck: "false" })),
          field(doc, "find-nodes", "Label (optional)", h(doc, "input", { id: "graph-label", spellcheck: "false" })),
          h(doc, "div", { class: "graph-equality", "data-graph-ops": "find-nodes" },
            field(doc, "find-nodes", "Property name (optional)", h(doc, "input", { id: "graph-eq-field", spellcheck: "false" })),
            field(doc, "find-nodes", "Value type", options(doc, "graph-eq-type", VALUE_TYPES)),
            field(doc, "find-nodes", "Equals", h(doc, "input", { id: "graph-eq-value", spellcheck: "false" }))),
          field(doc, "traverse", "Return only nodes with labels (comma separated)", h(doc, "input", { id: "graph-return-labels", spellcheck: "false" })),
          field(doc, "find-nodes traverse", "Node conditions (JSON array, e.g. [{\"op\":\"range\",\"field\":\"rank\",\"min\":{\"type\":\"integer\",\"value\":\"2\"}}])", h(doc, "textarea", { id: "graph-conditions", rows: "2", spellcheck: "false" })),
          field(doc, "traverse", "Expand only through labels (comma separated)", h(doc, "input", { id: "graph-expand-labels", spellcheck: "false" })),
          field(doc, "traverse", "Expand conditions (JSON array)", h(doc, "textarea", { id: "graph-expand-conditions", rows: "2", spellcheck: "false" })),
          field(doc, "neighbors traverse shortest-path", "Direction", options(doc, "graph-direction", DIRECTIONS, ["Outgoing", "Incoming", "Both"])),
          field(doc, "neighbors traverse shortest-path", "Edge relationship types (comma separated; blank = any)", h(doc, "input", { id: "graph-types", spellcheck: "false" })),
          field(doc, "neighbors traverse", "Edge conditions (JSON array)", h(doc, "textarea", { id: "graph-edge-conditions", rows: "2", spellcheck: "false" })),
          h(doc, "label", { "data-graph-ops": "traverse" }, h(doc, "input", { id: "graph-include-paths", type: "checkbox" }), "Include paths"),
          field(doc, "find-nodes neighbors", "Page size (blank = server default)", h(doc, "input", { id: "graph-limit", inputmode: "numeric" })),
          field(doc, "find-nodes neighbors", "Cursor (filled by Next page)", h(doc, "input", { id: "graph-cursor", spellcheck: "false", autocomplete: "off" })),
          h(doc, "details", { class: "advanced-settings" },
            h(doc, "summary", { text: "Limits and property projection" }),
            h(doc, "p", { text: "Blank limits use the server defaults. Values above the server ceiling are rejected with graph_limit_exceeds_ceiling, never clamped." }),
            h(doc, "div", { class: "graph-limits" }, limitFields),
            h(doc, "label", {}, "Properties returned", options(doc, "graph-projection", ["all", "none", "only"], ["All properties", "No properties", "Only the listed properties"])),
            h(doc, "label", {}, "Property names for “only” (comma separated)", h(doc, "input", { id: "graph-projection-fields", spellcheck: "false" }))),
          h(doc, "div", { class: "graph-actions" },
            h(doc, "button", { id: "graph-run", type: "button", text: "Run query" }),
            h(doc, "button", { id: "graph-explain", type: "button", class: "secondary", text: "Explain plan" }),
            h(doc, "button", { id: "graph-next", type: "button", class: "secondary", disabled: true, text: "Next page" }))),
        h(doc, "div", { class: "tool-group", id: "graph-results" },
          h(doc, "h3", { text: "Result" }),
          h(doc, "p", { id: "graph-summary", "aria-live": "polite", text: "Run a query to see nodes, edges and paths." }),
          table(doc, "graph-explain-table", ["Plan", "Value"]),
          h(doc, "figure", { class: "graph-figure", hidden: true },
            h(doc, "figcaption", { id: "graph-figure-caption", text: "Node-link view" })),
          table(doc, "graph-nodes", ["ID", "Key", "Labels", "Depth", "Properties"]),
          table(doc, "graph-edges", ["ID", "Key", "Relationship", "Source", "Target", "Properties"]),
          table(doc, "graph-paths", ["Path", "Nodes", "Edges"]),
          h(doc, "details", { class: "advanced-settings" }, h(doc, "summary", { text: "Response JSON" }), h(doc, "pre", { id: "graph-result" })))),
      h(doc, "div", { class: "tool-group", id: "graph-admin", hidden: true },
        h(doc, "div", { class: "section-heading" }, h(doc, "h3", { text: "Graph engine statistics (admin)" }),
          h(doc, "button", { id: "graph-stats", type: "button", class: "secondary", "data-route": STATS_ROUTE, text: "Load graph stats" })),
        h(doc, "p", { text: "Guard wait times, request counts and truncation reasons since the server started (GET /v1/admin/graph/stats)." }),
        h(doc, "div", { class: "stat-grid", id: "graph-stats-grid", hidden: true }),
        table(doc, "graph-truncations", ["Truncation reason", "Count"]),
        h(doc, "pre", { id: "graph-stats-result", "aria-live": "polite" })),
    ];
  }

  async function cookieRequest(win, method, path, payload) {
    const controller = typeof win.AbortController === "function" ? new win.AbortController() : null;
    const timer = controller ? win.setTimeout(() => controller.abort(), 15000) : null;
    try {
      const headers = { Accept: "application/json", "X-VLite-Request": "1" };
      const init = { method, headers, credentials: "same-origin", cache: "no-store" };
      if (controller) init.signal = controller.signal;
      if (payload !== undefined) { headers["Content-Type"] = "application/json"; init.body = JSON.stringify(payload); }
      const response = await win.fetch(path, init);
      const raw = await response.text();
      let value = null;
      if (raw) { try { value = JSON.parse(raw); } catch { value = raw; } }
      return { status: response.status, value };
    } catch (error) {
      const code = error && error.name === "AbortError" ? "timeout" : "network_error";
      return { status: 0, value: { error: { code, message: text(error && error.message) || code } } };
    } finally {
      if (timer !== null) win.clearTimeout(timer);
    }
  }

  function init(win) {
    const doc = win.document;
    const section = doc.getElementById("graph");
    if (!section || section.dataset.graphReady) return null;
    section.dataset.graphReady = "true";
    if (!doc.querySelector('link[href="/dashboard/graph.css"]') && doc.head) {
      doc.head.append(h(doc, "link", { rel: "stylesheet", href: "/dashboard/graph.css" }));
    }
    if (!section.hasAttribute("aria-labelledby")) section.setAttribute("aria-labelledby", "graph-title");
    section.replaceChildren(...markup(doc));
    const $ = (id) => doc.getElementById(id);
    const state = { status: "unknown", generation: 0, last: null };

    const send = async (method, path, payload) => {
      const bridge = win.VLiteGraphConnection;
      if (bridge && typeof bridge.request === "function") {
        try {
          const result = await bridge.request(method, path, payload);
          if (result && typeof result.status === "number") return result;
        } catch (error) {
          return { status: 0, value: { error: { code: "network_error", message: text(error && error.message) } } };
        }
      }
      return cookieRequest(win, method, path, payload);
    };

    const navLink = () => doc.querySelector('a[data-workspace="graph"]');
    const NOTES = {
      available: "Graphs are enabled. Reads are bounded by server limits; each result says whether it is complete.",
      denied: "No graph access. This session can reach the server but its credential has no graph_read permission or graph binding. Signed-in editor and viewer accounts cannot use graphs yet; connect with the admin or read-only token, or an API key granted graph_read for a graph.",
      disabled: "This database file has no property graphs enabled (graphs_not_enabled).",
      absent: "This server was built without property graph support.",
      "signed-out": "Connect or sign in to check graph access.",
      offline: "The server did not answer the graph check.",
    };

    const apply = (status, detail) => {
      state.status = status;
      const visible = VISIBLE.has(status);
      const link = navLink();
      if (link) link.hidden = !visible;
      $("graph-controls").hidden = status !== "available";
      $("graph-note").textContent = status === "error" ? `Graph check failed: ${detail}` : NOTES[status] || "";
      if (status !== "available") $("graph-admin").hidden = true;
    };

    const fillGraphs = (graphs) => {
      const select = $("graph-name");
      const current = select.value;
      select.replaceChildren(...graphs.map((graph) => h(doc, "option", { value: text(graph.name), text: text(graph.name) })));
      if (graphs.some((graph) => text(graph.name) === current)) select.value = current;
      $("graph-table").tBodies[0].replaceChildren(...graphs.map((graph) => h(doc, "tr", {},
        h(doc, "td", { text: text(graph.name) }), h(doc, "td", { text: text(graph.node_count) }), h(doc, "td", { text: text(graph.edge_count) }),
        h(doc, "td", {}, h(doc, "button", { type: "button", "data-graph-use": text(graph.name), text: "Use" })))));
      section.querySelector('[data-graph-table="graph-table"]').hidden = false;
      if (!graphs.length) $("graph-summary").textContent = "This file has graphs enabled but none you can read. Create one with the REST API, vlite-graph-cli or MCP.";
    };

    const probe = async () => {
      const generation = ++state.generation;
      const listed = await send("GET", "/v1/graphs");
      if (generation !== state.generation) return;
      let hasGraphPaths = false;
      if (listed.status === 403) {
        const spec = await send("GET", "/openapi.json");
        if (generation !== state.generation) return;
        hasGraphPaths = spec.status === 200 && openapiHasGraphs(spec.value);
      }
      const status = availability(listed.status, errorCode(listed.value), hasGraphPaths);
      apply(status, errorText(listed.status, listed.value));
      if (status !== "available") return;
      fillGraphs(object(listed.value) && Array.isArray(listed.value.graphs) ? listed.value.graphs : []);
      const capabilities = await send("GET", "/v1/capabilities");
      if (generation !== state.generation) return;
      $("graph-admin").hidden = !(capabilities.status === 200 && object(capabilities.value) && capabilities.value.role === "admin");
    };

    const showTable = (id, rows) => {
      const wrapper = section.querySelector(`[data-graph-table="${id}"]`);
      $(id).tBodies[0].replaceChildren(...rows);
      wrapper.hidden = rows.length === 0;
    };

    const cell = (value) => h(doc, "td", { text: text(value) });

    const draw = (result) => {
      const figure = section.querySelector(".graph-figure");
      const old = figure.querySelector("svg");
      if (old) old.remove();
      const view = layout(result);
      figure.hidden = view.nodes.length === 0;
      if (!view.nodes.length) return;
      const svg = doc.createElementNS(SVG_NS, "svg");
      const set = (element, attributes) => { for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, String(value)); return element; };
      const make = (tag, attributes) => set(doc.createElementNS(SVG_NS, tag), attributes);
      set(svg, { id: "graph-view", viewBox: `0 0 ${view.width} ${view.height}`, role: "img", "aria-label": `${view.nodes.length} nodes and ${view.edges.length} edges` });
      const defs = make("defs", {});
      const marker = make("marker", { id: "graph-arrow", viewBox: "0 0 8 8", refX: "8", refY: "4", markerWidth: "8", markerHeight: "8", orient: "auto-start-reverse" });
      marker.append(make("path", { d: "M0,0 L8,4 L0,8 z", class: "graph-arrow" }));
      defs.append(marker);
      svg.append(defs);
      const at = new Map(view.nodes.map((node) => [node.id, node]));
      const radius = 14;
      for (const edge of view.edges) {
        const from = at.get(edge.source);
        const to = at.get(edge.target);
        if (edge.source === edge.target) continue;
        const dx = to.x - from.x;
        const dy = to.y - from.y;
        const length = Math.hypot(dx, dy) || 1;
        const group = make("g", { class: "graph-edge" });
        group.append(make("line", { x1: from.x + (dx / length) * radius, y1: from.y + (dy / length) * radius,
          x2: to.x - (dx / length) * (radius + 2), y2: to.y - (dy / length) * (radius + 2), "marker-end": "url(#graph-arrow)" }));
        const label = make("text", { x: (from.x + to.x) / 2, y: (from.y + to.y) / 2 - 4, "text-anchor": "middle" });
        label.textContent = edge.label;
        group.append(label);
        svg.append(group);
      }
      for (const node of view.nodes) {
        const group = make("g", { class: "graph-node" });
        const title = make("title", {});
        title.textContent = `${node.key !== null ? `${node.key} ` : ""}(ID ${node.id})${node.labels.length ? ` ${node.labels.join(", ")}` : ""}`;
        group.append(title, make("circle", { cx: node.x, cy: node.y, r: radius }));
        const label = make("text", { x: node.x, y: node.y + radius + 14, "text-anchor": "middle" });
        label.textContent = node.text.length > 16 ? `${node.text.slice(0, 15)}…` : node.text;
        group.append(label);
        svg.append(group);
      }
      figure.prepend(svg);
      $("graph-figure-caption").textContent = view.omitted > 0
        ? `Node-link view of the first ${view.nodes.length} nodes (${view.omitted} more listed in the table).`
        : "Node-link view of the result.";
    };

    const clearResults = () => {
      for (const id of ["graph-explain-table", "graph-nodes", "graph-edges", "graph-paths"]) showTable(id, []);
      draw({ nodes: [], edges: [] });
    };

    const useNode = (id) => {
      $("graph-selector-kind").value = "id";
      $("graph-selector").value = id;
      $("graph-start").value = id;
    };

    const render = (value) => {
      const result = normalize(value);
      showTable("graph-nodes", result.nodes.map((node) => h(doc, "tr", {},
        h(doc, "td", {}, h(doc, "button", { type: "button", "data-graph-node": text(node.id), title: "Use this node ID in the query", text: text(node.id) })),
        cell(node.key), cell(Array.isArray(node.labels) ? node.labels.join(", ") : ""), cell(node.depth), cell(propertiesText(node.properties)))));
      showTable("graph-edges", result.edges.map((edge) => h(doc, "tr", {},
        cell(edge.id), cell(edge.key), cell(edge.relationship), cell(edge.source_key ?? edge.source), cell(edge.target_key ?? edge.target), cell(propertiesText(edge.properties)))));
      showTable("graph-paths", result.paths.map((path, index) => h(doc, "tr", {},
        cell(index + 1), cell((path.nodes || []).join(" → ")), cell((path.edges || []).join(", ")))));
      draw(result);
      let line = summary(value);
      if (result.missing) line = `Not found. ${line}`;
      if (result.outcome) line = `Shortest path: ${result.outcome}${result.outcomeDetail ? ` (${result.outcomeDetail})` : ""}. ${line}`;
      $("graph-summary").textContent = line;
    };

    const form = () => {
      const bounds = {};
      for (const name of LIMITS) bounds[name] = $(`graph-limit-${name}`).value;
      return {
        selectorKind: $("graph-selector-kind").value, selector: $("graph-selector").value, destination: $("graph-destination").value,
        start: $("graph-start").value, label: $("graph-label").value, eqField: $("graph-eq-field").value, eqType: $("graph-eq-type").value,
        eqValue: $("graph-eq-value").value, conditions: $("graph-conditions").value, returnLabels: $("graph-return-labels").value,
        expandLabels: $("graph-expand-labels").value, expandConditions: $("graph-expand-conditions").value, direction: $("graph-direction").value,
        types: $("graph-types").value, edgeConditions: $("graph-edge-conditions").value, includePaths: $("graph-include-paths").checked,
        limit: $("graph-limit").value, cursor: $("graph-cursor").value, limits: bounds, projection: $("graph-projection").value,
        projectionFields: $("graph-projection-fields").value,
      };
    };

    const SELECTOR_LABELS = { "get-node": "Node", "get-edge": "Edge", neighbors: "Node", "shortest-path": "Source node" };
    const showFields = () => {
      const operation = $("graph-operation").value;
      for (const element of section.querySelectorAll("[data-graph-ops]")) {
        const ops = element.getAttribute("data-graph-ops");
        element.hidden = ops !== "" && !ops.split(" ").includes(operation);
      }
      $("graph-selector-label").textContent = SELECTOR_LABELS[operation] || "Node";
      $("graph-next").disabled = true;
    };

    const busy = (flag) => { for (const id of ["graph-run", "graph-explain"]) $(id).disabled = flag; };

    const run = async (explain) => {
      const operation = $("graph-operation").value;
      let call;
      try {
        call = request(operation, $("graph-name").value, form(), explain);
      } catch (error) {
        $("graph-summary").textContent = `Error: ${error.message}`;
        return;
      }
      busy(true);
      $("graph-next").disabled = true;
      $("graph-summary").textContent = explain ? "Explaining…" : "Running…";
      try {
        const response = await send(call.method, call.path, call.body);
        $("graph-result").textContent = response.value === null ? "" : JSON.stringify(response.value, null, 2);
        clearResults();
        if (response.status !== 200) { $("graph-summary").textContent = errorText(response.status, response.value); return; }
        if (explain) {
          showTable("graph-explain-table", explainRows(response.value).map(([name, value]) => h(doc, "tr", {}, h(doc, "th", { scope: "row", text: name }), cell(value))));
          $("graph-summary").textContent = "Plan for the current form. Nothing was read.";
          return;
        }
        render(response.value);
        const next = object(response.value) ? text(response.value.cursor) : "";
        state.last = next && (operation === "find-nodes" || operation === "neighbors") ? next : null;
        $("graph-next").disabled = !state.last;
      } finally {
        busy(false);
      }
    };

    const loadStats = async () => {
      const response = await send("GET", STATS_ROUTE);
      $("graph-stats-result").textContent = response.status === 200 ? JSON.stringify(response.value, null, 2) : errorText(response.status, response.value);
      const grid = $("graph-stats-grid");
      if (response.status !== 200) { grid.hidden = true; showTable("graph-truncations", []); return; }
      grid.replaceChildren(...statsCards(response.value).map(([name, value]) => h(doc, "div", {}, h(doc, "span", { text: name }), h(doc, "strong", { text: value }))));
      grid.hidden = false;
      showTable("graph-truncations", truncationRows(response.value).map(([reason, total]) => h(doc, "tr", {}, cell(reason), cell(total))));
    };

    $("graph-refresh").addEventListener("click", probe);
    $("graph-list").addEventListener("click", probe);
    $("graph-operation").addEventListener("change", showFields);
    $("graph-run").addEventListener("click", () => { $("graph-cursor").value = ""; return run(false); });
    $("graph-explain").addEventListener("click", () => run(true));
    $("graph-next").addEventListener("click", () => { if (state.last) { $("graph-cursor").value = state.last; return run(false); } return undefined; });
    $("graph-stats").addEventListener("click", loadStats);
    section.addEventListener("click", (event) => {
      const target = event.target && event.target.closest ? event.target.closest("button") : null;
      if (!target) return;
      if (target.hasAttribute("data-graph-use")) $("graph-name").value = target.getAttribute("data-graph-use");
      if (target.hasAttribute("data-graph-node")) useNode(target.getAttribute("data-graph-node"));
    });
    win.addEventListener("vlite:connection", probe);
    win.addEventListener("hashchange", () => { if (win.location.hash === "#graph" && state.status !== "available") probe(); });
    showFields();
    apply("signed-out");
    probe();
    return { probe, state };
  }

  function boot(win) {
    if (!win || !win.document) return;
    if (win.document.readyState === "loading") win.document.addEventListener("DOMContentLoaded", () => init(win), { once: true });
    else init(win);
  }

  const api = { init, boot, utils: { graphPath, selector, selectorList, list, conditions, typedValue, equality, limits, projection, cursor, direction,
    body, request, errorCode, errorText, openapiHasGraphs, availability, truncationText, indexText, propertiesText, normalize, summary, explainRows,
    statsCards, truncationRows, layout, OPERATIONS, LIMITS, STATS_ROUTE } };
  if (typeof window !== "undefined") { window.VLiteGraphTools = api; boot(window); }
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})();
