"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

function element(id, tagName = "BUTTON") {
  const listeners = [];
  return { id, tagName, value: "", textContent: "", disabled: false, listeners,
    addEventListener(type, fn) { if (type === "click") listeners.push(fn); },
    click() { return Promise.all(listeners.map((fn) => fn())); } };
}
const ids = ["tenant-list", "tenant-name", "tenant-max-vectors", "tenant-create", "tenant-id", "tenant-delete", "tenant-result",
  "tenant-usage", "tenant-new-name", "tenant-limit-mode", "tenant-new-limit", "tenant-save-settings", "tenant-suspend", "tenant-resume",
  "tls-reload", "tls-result", "replication-after", "replication-max-bytes", "replication-export", "replication-download", "replication-result"];
const elements = Object.fromEntries(ids.map((id) => [id, element(id, id.endsWith("-result") ? "PRE" : "BUTTON")]));
const downloads = [];
const sandbox = {
  window: {}, URLSearchParams, Blob, setTimeout, BigInt, Number, JSON,
  URL: { createObjectURL: (blob) => { downloads.push(blob); return "blob:x"; }, revokeObjectURL() {} },
  document: {
    getElementById: (id) => elements[id] ?? null,
    createElement: () => ({ click() { downloads.push(this.download); }, remove() {} }),
    body: { append() {} },
  },
};
vm.createContext(sandbox);
vm.runInContext(fs.readFileSync("src/dashboard/admin-tools.js", "utf8"), sandbox);
const { init, utils } = sandbox.window.VLiteAdminTools;
const { tenantName, maxVectors, tenantId, tenantRequest, tenantPath, tenantList, tenantSettings, tenantUsage, identity, exportPath, exportSegment, exportFilename, availability, explain } = utils;

// Validators mirror the server's byte/control rules.
assert.equal(tenantName("a"), "a");
assert.equal(tenantName("x".repeat(64)), "x".repeat(64));
assert.equal(tenantName("é".repeat(32)), "é".repeat(32));
assert.equal(tenantName(" spaced "), " spaced ");
for (const bad of ["", "x".repeat(65), "é".repeat(33), "😀".repeat(17), "a\tb", "a\nb", "\u0000", "\u007f", "\u0085", "\ud800", "a\udc00", 5, null])
  assert.throws(() => tenantName(bad), undefined, JSON.stringify(bad));
assert.equal(tenantName("😀".repeat(16)), "😀".repeat(16));
assert.equal(maxVectors(""), null);
assert.equal(maxVectors("  "), null);
assert.equal(maxVectors("0"), 0);
assert.equal(maxVectors("9007199254740991"), 9007199254740991);
for (const bad of ["9007199254740992", "18446744073709551615", "18446744073709551616", "-1", "01", "1.5", "1e3", "abc", "0x10"])
  assert.throws(() => maxVectors(bad), undefined, bad);
assert.match(String((() => { try { maxVectors("18446744073709551615"); } catch (e) { return e.message; } })()), /cannot be sent exactly/);
assert.deepEqual({ ...tenantRequest("t", "") }, { name: "t", max_vectors: null });
const hex = "0123456789abcdef0123456789ABCDEF";
assert.equal(tenantId(` ${hex} `), hex.toLowerCase());
assert.equal(tenantPath(hex), `/v1/admin/tenants/${hex.toLowerCase()}`);
for (const bad of ["", "0123", `${hex}0`, "g".repeat(32), "../../v1/database".padEnd(32, "a")]) assert.throws(() => tenantId(bad));
const listed = tenantList({ items: [{ id: hex, name: "a", max_vectors: null }, { id: hex, name: "b", max_vectors: 2 ** 60 }] });
assert.equal(listed.count, 2);
assert.equal(listed.tenants[0].max_vectors, "unlimited");
assert.match(listed.tenants[1].max_vectors, /exceeds browser integer precision/);
assert.throws(() => tenantList({}));
// Lifecycle fields appear only when a newer server reports them.
const rich = tenantList({ items: [{ id: hex, name: "a", max_vectors: 5, state: "suspended", suspended_unix_ms: 0, collections: 2, vectors: 7 }] }).tenants[0];
assert.deepEqual({ ...rich }, { id: hex, name: "a", max_vectors: "5", state: "suspended (API keys blocked)", suspended_since: "1970-01-01T00:00:00.000Z", collections: 2, vectors: 7 });
assert.equal("state" in listed.tenants[0], false);

// Settings: blank name keeps it; limit choice must match the limit field.
assert.deepEqual({ ...tenantSettings("Renamed", "keep", "") }, { name: "Renamed" });
assert.deepEqual({ ...tenantSettings("", "set", " 0 ") }, { max_vectors: 0 });
assert.deepEqual({ ...tenantSettings("", "unlimited", "") }, { max_vectors: null });
assert.deepEqual({ ...tenantSettings("n", "set", "10") }, { name: "n", max_vectors: 10 });
for (const [name, mode, limit, pattern] of [["", "keep", "", /Nothing to change/], ["", "set", "", /Enter the new vector limit/],
  ["", "keep", "5", /not "Set a new limit"/], ["", "unlimited", "5", /not "Set a new limit"/], ["", "set", "9007199254740992", /cannot be sent exactly/],
  ["bad\nname", "keep", "", /control characters/], ["", "bogus", "", /Unknown vector limit/]])
  assert.throws(() => tenantSettings(name, mode, limit), pattern, `${name}/${mode}/${limit}`);

const usage = tenantUsage({ tenant: { id: hex, name: "a", max_vectors: 10, state: "active" }, collections: [{ name: "docs", physical_name: `t${hex}__docs`, vectors: 12 }],
  vectors: 12, max_vectors: 10, remaining_vectors: 0, over_quota: true, credentials: { active: 1, expired: 2, revoked: 3 } });
assert.equal(usage.vectors, 12);
assert.equal(usage.vector_limit, "10");
assert.equal(usage.remaining_vectors, 0);
assert.match(usage.over_limit, /^yes: new vectors are refused/);
assert.deepEqual({ ...usage.collections[0] }, { name: "docs", stored_as: `t${hex}__docs`, vectors: 12 });
assert.deepEqual({ ...usage.api_keys }, { active: 1, expired: 2, revoked: 3 });
const unlimited = tenantUsage({ tenant: { id: hex, name: "a", max_vectors: null }, collections: [], vectors: 0, max_vectors: null, remaining_vectors: null, over_quota: false, credentials: { active: 0, expired: 0, revoked: 0 } });
assert.equal(unlimited.remaining_vectors, "unlimited");
assert.equal(unlimited.over_limit, "no");
assert.throws(() => tenantUsage({ tenant: null, collections: [] }));

// Identity: display-only text from capabilities, with a fallback for older servers.
assert.deepEqual({ ...identity(null, false) }, { kind: "none", text: "Not signed in", detail: identity(null, false).detail });
assert.equal(identity({ role: "admin" }, false).text, "Not signed in");
const su = identity({ role: "admin", identity: { kind: "super_admin", label: "Super admin (bootstrap admin token)" } });
assert.equal(su.kind, "super_admin");
assert.equal(su.text, "Signed in as Super admin (bootstrap admin token)");
assert.match(su.detail, /manage tenants, API keys/);
const open = identity({ role: "admin", identity: { kind: "open", label: "Open access (no admin token configured; every caller is admin)" } });
assert.equal(open.text, "Open access (no admin token configured; every caller is admin)");
assert.match(open.detail, /Configure an admin token/);
const scoped = identity({ role: "scoped", scope: { operations: ["read", "search"], collections: ["docs"], tenant_id: hex },
  identity: { kind: "scoped", label: `API key "ci" (k1) · tenant "Acme"`, credential_id: "k1", credential_name: "ci", tenant_id: hex, tenant_name: "Acme" } });
assert.equal(scoped.text, `Signed in as API key "ci" (k1) · tenant "Acme"`);
assert.equal(scoped.detail, `Can read, search on collections docs inside tenant "Acme" (${hex}). Cannot administer the server.`);
assert.match(identity({ role: "scoped", scope: { operations: ["write"], collections: ["*"] } }).detail, /^Can write on all collections\. Cannot/);
const legacy = identity({ role: "read_only" });
assert.equal(legacy.kind, "read_only");
assert.match(legacy.text, /Read-only token \(this server does not report identity details\)/);
assert.match(identity({ role: "admin", identity: { label: "" } }).text, /Admin token \(this server/);
assert.match(identity({ role: "mystery" }).text, /Role "mystery"/);

assert.equal(exportPath("", ""), "/v1/admin/replication/export");
assert.equal(exportPath("18446744073709551615", "16777216"), "/v1/admin/replication/export?after_sequence=18446744073709551615&max_bytes=16777216");
for (const [after, max] of [["01", ""], ["18446744073709551616", ""], ["-1", ""], ["1&x=1", ""], ["", "0"], ["", "16777217"], ["", "1.5"]])
  assert.throws(() => exportPath(after, max), undefined, `${after}/${max}`);
const segmentResponse = { segment_base64: "AAECAw==", lineage: "abc/def", base_sequence: "0", last_sequence: "5", head_sequence: "9" };
const parsed = exportSegment(segmentResponse);
assert.equal(parsed.summary.segment_bytes, 4);
assert.equal(parsed.summary.more_available, true);
assert.equal(exportSegment({ ...segmentResponse, head_sequence: "5" }).summary.more_available, false);
assert.equal(exportFilename(parsed.envelope), "vlite-replication-abc_def-0-5.json");
for (const bad of [null, { ...segmentResponse, segment_base64: "A" }, { ...segmentResponse, last_sequence: 5 }, { ...segmentResponse, lineage: 1 }])
  assert.throws(() => exportSegment(bad));

// Gating: bootstrap admin plus real feature flags; replication by read-all reach.
const routes = ["/v1/admin/replication/export"];
const adminCaps = { role: "admin", features: { tenants: true, tls_reload: true, native_tls: true }, routes };
assert.equal(availability(adminCaps).tenants.allowed, true);
assert.equal(availability(adminCaps).tls.allowed, true);
assert.equal(availability(adminCaps, false).tenants.allowed, false);
const disabled = availability({ role: "admin", features: { tenants: false, tls_reload: false, native_tls: true }, routes: [] });
assert.match(disabled.tenants.reason, /VLITE_ADMIN_DIR/);
assert.match(disabled.tls.reason, /no file-backed TLS listener/);
assert.match(disabled.replication.reason, /does not expose/);
const ro = availability({ role: "read_only", features: { tenants: true, tls_reload: true }, routes });
assert.match(ro.tenants.reason, /bootstrap admin/);
assert.match(ro.tls.reason, /bootstrap admin/);
assert.equal(ro.replication.allowed, true);
assert.equal(availability({ role: "scoped", features: { tenants: true }, routes, scope: { operations: ["read"], collections: ["*"] } }).replication.allowed, true);
assert.equal(availability({ role: "scoped", features: { tenants: true }, routes, scope: { operations: ["read"], collections: ["docs"] } }).replication.allowed, false);
assert.equal(availability({ role: "scoped", features: { tenants: true }, routes, scope: { operations: ["read"], collections: ["*"] } }).tenants.allowed, false);
// Lifecycle needs tenants plus a server that advertises the lifecycle feature or routes.
assert.match(availability(adminCaps).lifecycle.reason, /cannot show tenant usage/);
assert.equal(availability({ ...adminCaps, features: { ...adminCaps.features, tenant_lifecycle: true } }).lifecycle.allowed, true);
assert.equal(availability({ ...adminCaps, routes: ["/v1/admin/tenants/{id}/usage"] }).lifecycle.allowed, true);
assert.match(ro.lifecycle.reason, /bootstrap admin/);
assert.match(disabled.lifecycle.reason, /VLITE_ADMIN_DIR/);
assert.match(availability(null, false).lifecycle.reason, /Connect/);
assert.match(explain(Object.assign(new Error("HTTP 403"), { code: "tenant_suspended", status: 403 })), /Resume the tenant first/);
assert.match(explain(Object.assign(new Error("HTTP 403"), { code: "tenant_unavailable", status: 403 })), /was deleted/);
assert.match(explain(Object.assign(new Error("HTTP 409: tenant_not_empty: x"), { code: "tenant_not_empty", status: 409 })), /Drop those collections first/);
assert.match(explain(Object.assign(new Error("HTTP 422"), { code: "tls_reload_failed" })), /previous certificates remain active/);

// Init wiring with a fake ctx following app.js semantics.
function fakeCtx() {
  const ctx = {
    calls: [], responses: [], errors: [], confirms: [], answer: true, isConnected: true, caps: adminCaps, statuses: [],
    connected: () => ctx.isConnected, capabilities: () => ctx.caps,
    api: async (method, path, body, options) => {
      ctx.calls.push({ method, path, body, options });
      const next = ctx.responses.shift();
      if (typeof next === "function") return next();
      if (next instanceof Error) throw next;
      return next;
    },
    confirm: async (message) => { ctx.confirms.push(message); return typeof ctx.answer === "function" ? ctx.answer() : ctx.answer; },
    run: async (fn) => { try { await fn(); } catch (error) { ctx.errors.push(error); } },
    status: (message) => ctx.statuses.push(message),
  };
  return ctx;
}
const reset = () => { for (const e of Object.values(elements)) { e.listeners.length = 0; e.value = ""; e.textContent = ""; e.disabled = false; } };

(async () => {
  // Missing markup fails loudly; init itself issues no requests.
  {
    const saved = elements["tls-result"]; delete elements["tls-result"];
    assert.throws(() => init(fakeCtx()), /#tls-result/);
    elements["tls-result"] = saved; reset();
  }
  let ctx = fakeCtx();
  let tools = init(ctx);
  assert.deepEqual(Object.keys(tools).sort(), ["capabilities", "disconnect", "gate"]);
  tools.capabilities(adminCaps);
  assert.equal(ctx.calls.length, 0);
  assert.equal(elements["replication-export"].disabled, false);
  assert.equal(elements["replication-download"].disabled, true);

  // Tenant list and create happy path.
  ctx.responses.push({ items: [{ id: hex.toLowerCase(), name: "acme", max_vectors: 10 }] });
  await elements["tenant-list"].click();
  assert.deepEqual(ctx.calls.at(-1).path, "/v1/admin/tenants");
  assert.match(elements["tenant-result"].textContent, /acme/);
  elements["tenant-name"].value = "team é";
  elements["tenant-max-vectors"].value = "1000";
  ctx.responses.push({ tenant: { id: "f".repeat(32), name: "team é", max_vectors: 1000 } });
  await elements["tenant-create"].click();
  assert.deepEqual({ ...ctx.calls.at(-1).body }, { name: "team é", max_vectors: 1000 });
  assert.equal(ctx.calls.at(-1).method, "POST");
  assert.match(ctx.confirms.at(-1), /aggregate limit of 1000/);
  assert.equal(elements["tenant-id"].value, "f".repeat(32));

  // Unsafe input never reaches the API.
  const before = ctx.calls.length;
  elements["tenant-max-vectors"].value = "18446744073709551615";
  await elements["tenant-create"].click();
  elements["tenant-max-vectors"].value = "";
  elements["tenant-name"].value = "bad\nname";
  await elements["tenant-create"].click();
  elements["tenant-id"].value = "../database";
  await elements["tenant-delete"].click();
  assert.equal(ctx.calls.length, before);
  assert.match(elements["tenant-result"].textContent, /Tenant ID must be/);

  // Confirmation declined sends nothing.
  ctx.answer = false;
  elements["tenant-id"].value = hex;
  await elements["tenant-delete"].click();
  await elements["tls-reload"].click();
  assert.equal(ctx.calls.length, before);
  assert.match(ctx.confirms.at(-2), /tenant_not_empty/);
  assert.match(ctx.confirms.at(-1), /Nothing is uploaded/);
  ctx.answer = true;

  // Delete error explains nonempty rejection.
  ctx.responses.push(Object.assign(new Error("HTTP 409: tenant_not_empty: drop tenant collections before deleting the tenant"), { status: 409, code: "tenant_not_empty" }));
  await elements["tenant-delete"].click();
  assert.equal(ctx.calls.at(-1).method, "DELETE");
  assert.equal(ctx.calls.at(-1).path, `/v1/admin/tenants/${hex.toLowerCase()}`);
  assert.match(elements["tenant-result"].textContent, /Drop those collections first/);
  ctx.responses.push({ deleted: true });
  await elements["tenant-delete"].click();
  assert.match(elements["tenant-result"].textContent, /"deleted": true/);

  // Lifecycle: older servers get an explanation and no request.
  let lifecycleCount = ctx.calls.length;
  elements["tenant-id"].value = hex;
  await elements["tenant-usage"].click();
  assert.equal(ctx.calls.length, lifecycleCount);
  assert.match(elements["tenant-result"].textContent, /cannot show tenant usage/);
  ctx.caps = { ...adminCaps, features: { ...adminCaps.features, tenant_lifecycle: true } };
  const lower = hex.toLowerCase();
  ctx.responses.push({ tenant: { id: lower, name: "acme", max_vectors: 10, state: "active" }, collections: [], vectors: 3, max_vectors: 10, remaining_vectors: 7, over_quota: false, credentials: { active: 1, expired: 0, revoked: 0 } });
  await elements["tenant-usage"].click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path, ctx.calls.at(-1).body], ["GET", `/v1/admin/tenants/${lower}/usage`, undefined]);
  assert.match(elements["tenant-result"].textContent, /"remaining_vectors": 7/);
  assert.match(elements["tenant-result"].textContent, /"active": 1/);

  // Settings: invalid choices send nothing; a confirmed save sends PUT and clears the form.
  lifecycleCount = ctx.calls.length;
  await elements["tenant-save-settings"].click();
  assert.match(elements["tenant-result"].textContent, /Nothing to change/);
  elements["tenant-new-limit"].value = "50";
  elements["tenant-limit-mode"].value = "keep";
  await elements["tenant-save-settings"].click();
  assert.match(elements["tenant-result"].textContent, /not "Set a new limit"/);
  assert.equal(ctx.calls.length, lifecycleCount);
  elements["tenant-new-name"].value = "Acme Prod";
  elements["tenant-limit-mode"].value = "set";
  ctx.answer = false;
  await elements["tenant-save-settings"].click();
  assert.equal(ctx.calls.length, lifecycleCount);
  assert.match(ctx.confirms.at(-1), /rename it to "Acme Prod" and set its vector limit to 50/);
  ctx.answer = true;
  ctx.responses.push({ tenant: { id: lower, name: "Acme Prod", max_vectors: 50, state: "active" } });
  await elements["tenant-save-settings"].click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path, { ...ctx.calls.at(-1).body }], ["PUT", `/v1/admin/tenants/${lower}/settings`, { name: "Acme Prod", max_vectors: 50 }]);
  assert.match(elements["tenant-result"].textContent, /"name": "Acme Prod"/);
  assert.deepEqual([elements["tenant-new-name"].value, elements["tenant-limit-mode"].value, elements["tenant-new-limit"].value], ["", "keep", ""]);
  elements["tenant-limit-mode"].value = "unlimited";
  ctx.responses.push({ tenant: { id: lower, name: "Acme Prod", max_vectors: null, state: "active" } });
  await elements["tenant-save-settings"].click();
  assert.deepEqual({ ...ctx.calls.at(-1).body }, { max_vectors: null });
  assert.match(ctx.confirms.at(-1), /remove its vector limit/);

  // Suspend and resume are confirmed, body-less POSTs; server errors are explained.
  ctx.responses.push({ tenant: { id: lower, name: "Acme Prod", max_vectors: null, state: "suspended", suspended_unix_ms: 1000 } });
  await elements["tenant-suspend"].click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path, ctx.calls.at(-1).body], ["POST", `/v1/admin/tenants/${lower}/suspend`, undefined]);
  assert.match(ctx.confirms.at(-1), /Every API key bound to it is refused/);
  assert.match(elements["tenant-result"].textContent, /suspended \(API keys blocked\)/);
  ctx.responses.push({ tenant: { id: lower, name: "Acme Prod", max_vectors: null, state: "active" } });
  await elements["tenant-resume"].click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path], ["POST", `/v1/admin/tenants/${lower}/resume`]);
  assert.match(elements["tenant-result"].textContent, /"state": "active"/);
  ctx.responses.push(Object.assign(new Error("HTTP 404: tenant_not_found"), { status: 404, code: "tenant_not_found" }));
  await elements["tenant-resume"].click();
  assert.match(elements["tenant-result"].textContent, /No live tenant has this ID/);
  ctx.caps = adminCaps;

  // TLS reload uses the body-less POST schema.
  ctx.responses.push({ reloaded: 2 });
  await elements["tls-reload"].click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path, ctx.calls.at(-1).body], ["POST", "/v1/admin/tls/reload", undefined]);
  assert.match(elements["tls-result"].textContent, /"reloaded": 2/);
  ctx.responses.push(Object.assign(new Error("HTTP 422: tls_reload_failed"), { status: 422, code: "tls_reload_failed" }));
  await elements["tls-reload"].click();
  assert.match(elements["tls-result"].textContent, /previous certificates remain active/);

  // Replication export, continuation and download.
  elements["replication-max-bytes"].value = "1024";
  ctx.responses.push(segmentResponse);
  await elements["replication-export"].click();
  assert.equal(ctx.calls.at(-1).path, "/v1/admin/replication/export?max_bytes=1024");
  assert.equal(elements["replication-after"].value, "5");
  assert.equal(elements["replication-download"].disabled, false);
  assert.doesNotMatch(elements["replication-result"].textContent, /AAECAw/);
  await elements["replication-download"].click();
  assert.equal(downloads.at(-1), "vlite-replication-abc_def-0-5.json");

  // Disconnect during a pending request: stale result is discarded and outputs cleared.
  let release;
  ctx.responses.push(() => new Promise((resolve) => { release = resolve; }));
  const pending = elements["tenant-list"].click();
  await new Promise((resolve) => setImmediate(resolve));
  tools.disconnect();
  ctx.isConnected = false;
  release({ items: [{ id: hex, name: "late", max_vectors: null }] });
  await pending;
  assert.equal(elements["tenant-result"].textContent, "");
  assert.equal(elements["replication-result"].textContent, "");
  assert.equal(elements["replication-after"].value, "");
  assert.equal(elements["replication-export"].disabled, true);
  assert.equal(elements["replication-download"].disabled, true);

  // Disconnect while confirming: nothing is sent after confirmation.
  ctx.isConnected = true;
  tools.capabilities(adminCaps);
  const sent = ctx.calls.length;
  ctx.answer = () => { tools.disconnect(); return true; };
  await elements["tls-reload"].click();
  assert.equal(ctx.calls.length, sent);
  assert.equal(elements["tls-result"].textContent, "");
  ctx.answer = true;

  // Stale errors from ctx.api are not written.
  ctx.responses.push(Object.assign(new Error("Connection or collection changed; response discarded"), { stale: true }));
  await elements["tls-reload"].click();
  assert.equal(elements["tls-result"].textContent, "");

  // Gating: non-admin and disabled features get explicit messages and no requests.
  const count = ctx.calls.length;
  ctx.caps = { role: "read_only", features: { tenants: true, tls_reload: true }, routes };
  tools.capabilities(ctx.caps);
  assert.match(elements["tenant-result"].textContent, /bootstrap admin/);
  assert.match(elements["tls-result"].textContent, /bootstrap admin/);
  assert.equal(elements["replication-result"].textContent, "");
  await elements["tenant-list"].click();
  await elements["tls-reload"].click();
  await elements["tenant-suspend"].click();
  assert.equal(ctx.calls.length, count);
  ctx.caps = { role: "admin", features: { tenants: false, tls_reload: false, native_tls: true }, routes: [] };
  tools.capabilities(ctx.caps);
  assert.match(elements["tenant-result"].textContent, /disabled/);
  assert.match(elements["tls-result"].textContent, /no file-backed TLS listener/);
  assert.match(elements["replication-result"].textContent, /does not expose/);
  assert.equal(elements["replication-export"].disabled, true);
  await elements["tenant-create"].click();
  await elements["replication-export"].click();
  assert.equal(ctx.calls.length, count);
  console.log("admin tools tests passed");
})().catch((error) => { console.error(error); process.exit(1); });
