"use strict";
// Bootstrap-admin lifecycle tools: tenants, TLS reload and replication log export.
// Pure validators are exported for Node tests; init(ctx) only registers listeners.
(() => {
  const U64_MAX = 18446744073709551615n;
  const EXPORT_ROUTE = "/v1/admin/replication/export";
  const EXPORT_MAX_BYTES = 16777216;
  const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;
  const LONE_SURROGATE = /[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/;
  const object = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
  const utf8Length = (value) => {
    let bytes = 0;
    for (const char of value) {
      const point = char.codePointAt(0);
      bytes += point < 0x80 ? 1 : point < 0x800 ? 2 : point < 0x10000 ? 3 : 4;
    }
    return bytes;
  };
  const canonicalU64 = (value) => typeof value === "string" && /^(0|[1-9][0-9]{0,19})$/.test(value) && BigInt(value) <= U64_MAX;

  // Mirrors the server: 1..64 UTF-8 bytes, no Unicode control characters (char::is_control), no trimming.
  function tenantName(value) {
    if (typeof value !== "string" || value === "") throw new Error("Tenant name is required (1..64 UTF-8 bytes); no request sent");
    if (LONE_SURROGATE.test(value)) throw new Error("Tenant name contains invalid Unicode; no request sent");
    if (CONTROL.test(value)) throw new Error("Tenant name must not contain control characters (tabs, newlines, U+0000..U+001F, U+007F..U+009F); no request sent");
    const bytes = utf8Length(value);
    if (bytes > 64) throw new Error(`Tenant name is ${bytes} UTF-8 bytes; the server allows at most 64. No request sent`);
    return value;
  }
  // Optional u64 limit. Values above 2^53-1 are valid on the server but cannot be sent exactly
  // from browser JSON, so they are rejected rather than silently rounded.
  function maxVectors(value) {
    const text = typeof value === "string" ? value.trim() : value;
    if (text === "" || text === null || text === undefined) return null;
    if (typeof text !== "string" || !/^(0|[1-9][0-9]*)$/.test(text)) throw new Error("Max vectors must be blank (unlimited) or a canonical non-negative integer; no request sent");
    const big = BigInt(text);
    if (big > U64_MAX) throw new Error("Max vectors exceeds the u64 maximum 18446744073709551615; no request sent");
    if (big > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error(`Max vectors above ${Number.MAX_SAFE_INTEGER} cannot be sent exactly from the dashboard; leave it blank (unlimited) or use the REST API with an exact integer. No request sent`);
    return Number(big);
  }
  function tenantId(value) {
    const text = typeof value === "string" ? value.trim() : "";
    if (!/^[0-9a-fA-F]{32}$/.test(text)) throw new Error("Tenant ID must be the 32-character hexadecimal ID returned by List or Create; no request sent");
    return text.toLowerCase();
  }
  const tenantRequest = (name, limit) => ({ name: tenantName(name), max_vectors: maxVectors(limit) });
  const tenantPath = (value) => `/v1/admin/tenants/${tenantId(value)}`;
  const limitText = (limit) => limit === null || limit === undefined ? "unlimited"
    : Number.isSafeInteger(limit) ? String(limit) : `about ${limit} (exceeds browser integer precision; inspect via the REST API)`;
  function tenantList(value) {
    if (!object(value) || !Array.isArray(value.items)) throw new Error("Unexpected tenant list response");
    return { count: value.items.length, tenants: value.items.map((item) => ({ id: item?.id, name: item?.name, max_vectors: limitText(item?.max_vectors) })) };
  }

  function exportPath(after, maxBytes) {
    const params = new URLSearchParams();
    const sequence = typeof after === "string" ? after.trim() : "";
    if (sequence !== "") {
      if (!canonicalU64(sequence)) throw new Error("After sequence must be blank (from the beginning) or a canonical decimal u64; no request sent");
      params.set("after_sequence", sequence);
    }
    const bytes = typeof maxBytes === "string" ? maxBytes.trim() : maxBytes === undefined || maxBytes === null ? "" : String(maxBytes);
    if (bytes !== "") {
      if (!/^[1-9][0-9]{0,7}$/.test(bytes) || Number(bytes) > EXPORT_MAX_BYTES) throw new Error(`Max bytes must be 1..${EXPORT_MAX_BYTES}; no request sent`);
      params.set("max_bytes", bytes);
    }
    const query = params.toString();
    return query ? `${EXPORT_ROUTE}?${query}` : EXPORT_ROUTE;
  }
  function exportSegment(value) {
    if (!object(value) || typeof value.segment_base64 !== "string" || typeof value.lineage !== "string"
      || !["base_sequence", "last_sequence", "head_sequence"].every((key) => canonicalU64(value[key]))
      || !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(value.segment_base64)) throw new Error("Unexpected replication export response; nothing kept");
    const padding = value.segment_base64.endsWith("==") ? 2 : value.segment_base64.endsWith("=") ? 1 : 0;
    const bytes = value.segment_base64.length / 4 * 3 - padding;
    const remaining = BigInt(value.head_sequence) > BigInt(value.last_sequence);
    return {
      envelope: { segment_base64: value.segment_base64, lineage: value.lineage, base_sequence: value.base_sequence, last_sequence: value.last_sequence, head_sequence: value.head_sequence },
      summary: { lineage: value.lineage, base_sequence: value.base_sequence, last_sequence: value.last_sequence, head_sequence: value.head_sequence, segment_bytes: bytes,
        more_available: remaining,
        next: remaining ? `Export again with after_sequence=${value.last_sequence} to continue (filled in).` : "Up to date with the server head at export time." },
    };
  }
  const exportFilename = (envelope) => `vlite-replication-${envelope.lineage.replace(/[^A-Za-z0-9-]/g, "_").slice(0, 64)}-${envelope.base_sequence}-${envelope.last_sequence}.json`;

  // Pure gating from /v1/capabilities; reasons are shown verbatim in result outputs.
  function availability(caps, connected = true) {
    const role = caps?.role;
    const features = object(caps?.features) ? caps.features : {};
    const routes = Array.isArray(caps?.routes) ? caps.routes : [];
    const offline = { allowed: false, reason: "Connect to a server first." };
    if (!connected || !object(caps)) return { tenants: offline, tls: offline, replication: offline };
    const admin = role === "admin";
    const notAdmin = (what) => `${what} requires the bootstrap admin token (current role: ${role ?? "unknown"}). Read-only and scoped credentials cannot use it.`;
    const tenants = !admin ? { allowed: false, reason: notAdmin("Tenant management") }
      : features.tenants !== true ? { allowed: false, reason: "Tenants are disabled on this server: they require admin token authentication plus an admin directory (VLITE_ADMIN_DIR) at startup." }
      : { allowed: true, reason: "" };
    const tls = !admin ? { allowed: false, reason: notAdmin("TLS reload") }
      : features.tls_reload !== true ? { allowed: false, reason: features.native_tls === true
        ? "TLS reload is unavailable: no file-backed TLS listener is configured (start with --tls-cert/--tls-key or gRPC TLS files). Plain HTTP servers have nothing to reload."
        : "This server does not advertise TLS reload support." }
      : { allowed: true, reason: "" };
    const scope = caps.scope;
    const readAll = admin || role === "read_only" || (role === "scoped" && Array.isArray(scope?.operations) && scope.operations.includes("read") && Array.isArray(scope?.collections) && scope.collections.includes("*"));
    const replication = !routes.includes(EXPORT_ROUTE) ? { allowed: false, reason: "This server does not expose replication log export." }
      : !readAll ? { allowed: false, reason: `Replication export requires admin, read-only, or a scoped credential with read on all collections ("*") (current role: ${role ?? "unknown"}).` }
      : { allowed: true, reason: "" };
    return { tenants, tls, replication };
  }

  const HINTS = {
    tenants_disabled: "Tenants are disabled: configure admin token authentication and VLITE_ADMIN_DIR, then restart.",
    tenant_not_found: "No live tenant has this ID (it may already be deleted). List tenants to refresh.",
    tenant_not_empty: "The tenant still owns collections (named t<tenant-id>__*). Drop those collections first with a credential scoped to the tenant or as admin, then delete again.",
    tenants_commit_indeterminate: "The change is in effect but its durability was not confirmed. List tenants before retrying; do not repeat blindly.",
    tls_disabled: "No file-backed TLS listener is configured; nothing was reloaded.",
    tls_reload_failed: "The TLS files on the server are invalid or unreadable. Nothing changed: the previous certificates remain active. Fix the files on the server host and retry.",
    invalid_input: "The server rejected the input; nothing was changed.",
    invalid_query: "The server rejected the export parameters; nothing was exported.",
    invalid_id: "After sequence must be a canonical decimal u64.",
  };
  function explain(error) {
    const message = String(error?.message ?? error);
    const hint = HINTS[error?.code] || (error?.status === 403 ? "This credential is not permitted to perform this operation." : "");
    return hint ? `${message}\n${hint}` : message;
  }

  function init(ctx) {
    const $ = (id) => document.getElementById(id);
    const controls = ["tenant-list", "tenant-name", "tenant-max-vectors", "tenant-create", "tenant-id", "tenant-delete", "tenant-result",
      "tls-reload", "tls-result", "replication-after", "replication-max-bytes", "replication-export", "replication-download", "replication-result"];
    for (const name of controls) if (!$(name)) throw new Error(`admin markup is missing control #${name}`);
    const OUTPUTS = { tenants: "tenant-result", tls: "tls-result", replication: "replication-result" };
    let epoch = 0;
    let segment = null;
    let gates = availability(null, false);
    const show = (id, value) => { $(id).textContent = typeof value === "string" ? value : JSON.stringify(value, null, 2); };
    const current = (mark) => mark === epoch && ctx.connected();
    const need = (area) => {
      gates = availability(ctx.capabilities(), ctx.connected());
      if (!gates[area].allowed) { show(OUTPUTS[area], gates[area].reason); throw new Error(gates[area].reason); }
    };
    // Every action captures the epoch; disconnect/reconnect bumps it so late results are dropped.
    const on = (name, area, action) => $(name).addEventListener("click", () => ctx.run(async () => {
      const mark = epoch;
      try { await action(mark); }
      catch (error) {
        if (!error?.stale && current(mark)) show(OUTPUTS[area], explain(error));
        throw error;
      }
    }));
    const listTenants = async (mark) => {
      const value = await ctx.api("GET", "/v1/admin/tenants", undefined, { data: false });
      if (current(mark)) show("tenant-result", tenantList(value));
    };
    on("tenant-list", "tenants", async (mark) => { need("tenants"); await listTenants(mark); });
    on("tenant-create", "tenants", async (mark) => {
      need("tenants");
      const body = tenantRequest($("tenant-name").value, $("tenant-max-vectors").value);
      if (!await ctx.confirm(`Create tenant "${body.name}" with ${body.max_vectors === null ? "no vector limit" : `an aggregate limit of ${body.max_vectors} vectors`}? The server assigns a random tenant ID; the change is durable and audited. Names are not unique.`)) return;
      if (!current(mark)) return;
      const value = await ctx.api("POST", "/v1/admin/tenants", body, { data: false });
      if (!current(mark)) return;
      show("tenant-result", { created: value?.tenant ? { ...value.tenant, max_vectors: limitText(value.tenant.max_vectors) } : value, note: "Use this tenant ID when issuing scoped credentials." });
      if (value?.tenant?.id) $("tenant-id").value = value.tenant.id;
    });
    on("tenant-delete", "tenants", async (mark) => {
      need("tenants");
      const value = tenantId($("tenant-id").value);
      if (!await ctx.confirm(`Delete tenant ${value}? The server refuses (HTTP 409 tenant_not_empty) while any collection named t${value}__* exists; drop those collections first. Credentials scoped to this tenant stop working, and the dashboard cannot undo the deletion.`)) return;
      if (!current(mark)) return;
      const result = await ctx.api("DELETE", `/v1/admin/tenants/${value}`, undefined, { data: false });
      if (current(mark)) show("tenant-result", { tenant_id: value, ...(object(result) ? result : { result }) });
    });
    on("tls-reload", "tls", async (mark) => {
      need("tls");
      if (!await ctx.confirm("Reload TLS certificates from the files configured at server startup for every TLS listener? Nothing is uploaded from this browser. All files are validated first; on any error the previous certificates stay active. Existing connections keep their current identity; only new handshakes use the reloaded certificate.")) return;
      if (!current(mark)) return;
      const value = await ctx.api("POST", "/v1/admin/tls/reload", undefined, { data: false });
      if (current(mark)) show("tls-result", { ...(object(value) ? value : { result: value }), note: "New TLS handshakes now use the reloaded identity; existing connections are unchanged." });
    });
    on("replication-export", "replication", async (mark) => {
      need("replication");
      const path = exportPath($("replication-after").value, $("replication-max-bytes").value);
      segment = null; gate();
      const value = await ctx.api("GET", path, undefined, { data: false });
      if (!current(mark)) return;
      const parsed = exportSegment(value);
      segment = parsed.envelope;
      if (parsed.summary.more_available) $("replication-after").value = parsed.envelope.last_sequence;
      show("replication-result", { ...parsed.summary, download: "Use Download to save this segment as JSON (contains database contents)." });
      gate();
    });
    on("replication-download", "replication", async () => {
      if (!segment) throw new Error("Export a segment first");
      const url = URL.createObjectURL(new Blob([JSON.stringify(segment)], { type: "application/json" }));
      const link = document.createElement("a");
      link.href = url; link.download = exportFilename(segment);
      document.body.append(link); link.click(); link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 0);
      ctx.status?.(`Saved ${link.download}`);
    });
    function gate(offline = false) {
      gates = availability(ctx.capabilities(), !offline && ctx.connected());
      $("replication-export").disabled = !gates.replication.allowed;
      $("replication-download").disabled = !gates.replication.allowed || segment === null;
    }
    function reset() {
      epoch++;
      segment = null;
      for (const id of Object.values(OUTPUTS)) show(id, "");
    }
    return {
      capabilities(caps) {
        reset();
        gates = availability(caps, ctx.connected());
        for (const [area, id] of Object.entries(OUTPUTS)) if (!gates[area].allowed) show(id, gates[area].reason);
        gate();
      },
      disconnect() {
        reset();
        $("replication-after").value = "";
        gate(true);
      },
      gate,
    };
  }

  const api = { init, utils: { tenantName, maxVectors, tenantId, tenantRequest, tenantPath, tenantList, exportPath, exportSegment, exportFilename, availability, explain, utf8Length } };
  if (typeof window !== "undefined") window.VLiteAdminTools = api;
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})();
