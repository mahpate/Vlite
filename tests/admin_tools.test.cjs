"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

function element(id, tagName = "BUTTON") {
  const listeners = [];
  const inputs = [];
  return { id, tagName, value: "", textContent: "", disabled: false, checked: false, hidden: false, attributes: {}, children: [], focused: 0, listeners, inputs,
    setAttribute(name, value) { this.attributes[name] = String(value); }, replaceChildren(...nodes) { this.children = nodes; },
    append(...nodes) { this.children.push(...nodes); }, focus() { this.focused++; },
    addEventListener(type, fn) { if (type === "click") listeners.push(fn); else if (type === "input") inputs.push(fn); },
    click() { return Promise.all(listeners.map((fn) => fn())); },
    input() { inputs.forEach((fn) => fn()); } };
}
const ids = ["tenant-list", "tenant-name", "tenant-max-vectors", "tenant-create", "tenant-id", "tenant-delete", "tenant-result",
  "tenant-usage", "tenant-new-name", "tenant-limit-mode", "tenant-new-limit", "tenant-save-settings", "tenant-suspend", "tenant-resume",
  "tls-reload", "tls-result", "replication-after", "replication-max-bytes", "replication-export", "replication-download", "replication-result",
  "user-list", "user-id", "user-username", "user-role", "user-tenant", "user-password", "user-must-change", "user-disabled",
  "user-create", "user-save", "user-reset-password", "user-delete", "user-result",
  "compaction-status", "compaction-facts", "compaction-reasons", "compaction-refresh", "compaction-start", "compaction-cancel", "compaction-start-help",
  "compaction-ack", "compaction-ack-error", "compaction-ack-downtime", "compaction-ack-history", "compaction-enabled", "compaction-history-policy",
  "compaction-history-policy-error", "compaction-start-utc", "compaction-start-utc-error", "compaction-local-time", "compaction-window", "compaction-window-error",
  "compaction-max-run", "compaction-max-run-error", "compaction-min-file", "compaction-min-file-error", "compaction-min-growth", "compaction-min-growth-error",
  "compaction-growth-percent", "compaction-growth-percent-error", "compaction-cooldown", "compaction-cooldown-error", "compaction-max-source",
  "compaction-max-source-error", "compaction-save", "compaction-result"];
const elements = Object.fromEntries(ids.map((id) => [id, element(id, id.endsWith("-result") ? "PRE" : "BUTTON")]));
const downloads = [];
// Polling timers (5 s and longer) are captured so tests advance them by hand.
const timers = [];
const sandbox = {
  window: {}, URLSearchParams, Blob, BigInt, Number, JSON, Uint8Array, Date, crypto: globalThis.crypto,
  setTimeout: (fn, ms) => { if (ms >= 5000) { timers.push({ fn, ms }); return timers.length; } return setTimeout(fn, ms); },
  clearTimeout: (id) => { if (typeof id === "number") { if (timers[id - 1]) timers[id - 1].fn = null; } else clearTimeout(id); },
  URL: { createObjectURL: (blob) => { downloads.push(blob); return "blob:x"; }, revokeObjectURL() {} },
  document: {
    getElementById: (id) => elements[id] ?? null,
    createElement: (tag) => ({ ...element("", tag.toUpperCase()), click() { downloads.push(this.download); }, remove() {} }),
    body: { append() {} },
  },
};
vm.createContext(sandbox);
vm.runInContext(fs.readFileSync("src/dashboard/admin-tools.js", "utf8"), sandbox);
const { init, utils } = sandbox.window.VLiteAdminTools;
const { tenantName, maxVectors, tenantId, tenantRequest, tenantPath, tenantList, tenantSettings, tenantUsage, identity, exportPath, exportSegment, exportFilename, availability, explain,
  username, password, userRole, userTenant, userId, userRequest, userChanges, userView, userList } = utils;

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
// Password accounts: role-specific text, tenant placement and a pending password change.
const lowerHex = hex.toLowerCase();
const editor = identity({ role: "scoped", identity: { kind: "user", label: "ann (Editor)", username: "ann", account_role: "editor", tenant_id: lowerHex, tenant_name: "Acme", must_change_password: false } });
assert.equal(editor.kind, "user");
assert.equal(editor.text, "Signed in as ann (Editor)");
assert.equal(editor.detail, `Can read, search and change data inside tenant "Acme" (${lowerHex}). Cannot administer the server.`);
const boss = identity({ role: "admin", identity: { kind: "user", label: "root (Super admin)", account_role: "super_admin", must_change_password: true } });
assert.match(boss.detail, /^Full administrator: can manage users, tenants/);
assert.match(boss.detail, /Choose a new password to continue\.$/);
assert.match(identity({ role: "read_only", identity: { kind: "user", label: "v", account_role: "viewer" } }).detail, /^Can read and search in every collection\. Cannot change data/);

// User validators mirror the server's account rules; nothing invalid is sent.
assert.equal(username(" Alice.Ops@x "), "alice.ops@x");
for (const bad of ["", "ab", ".alice", "a b c", "x".repeat(65), "al\nice", 7, null]) assert.throws(() => username(bad), /Username must be/, JSON.stringify(bad));
const strong = "correct horse battery staple";
assert.equal(password(strong), strong);
assert.equal(password("😀".repeat(15)), "😀".repeat(15));
for (const [bad, pattern] of [["", /required/], [5, /required/], ["x".repeat(14), /15 to 128/], ["x".repeat(129), /15 to 128/], ["😀".repeat(129), /it has 129/],
  [`${"x".repeat(15)}\n`, /control characters/], [`${"x".repeat(15)}\ud800`, /invalid Unicode/]])
  assert.throws(() => password(bad), pattern, JSON.stringify(bad));
assert.equal(userRole("editor"), "editor");
for (const bad of ["admin", "", "toString", "__proto__", undefined]) assert.throws(() => userRole(bad), /Choose a role/, String(bad));
assert.equal(userTenant("  ", "editor"), null);
assert.equal(userTenant("", "super_admin"), null);
assert.equal(userTenant(` ${hex} `, "viewer"), lowerHex);
assert.throws(() => userTenant(hex, "super_admin"), /cannot belong to a tenant/);
assert.throws(() => userTenant("../x", "editor"), /Tenant ID must be/);
assert.equal(userId(` ${hex} `), lowerHex);
for (const bad of ["", "abc", `${hex}0`, "../../v1/database".padEnd(32, "a"), null]) assert.throws(() => userId(bad), /User ID must be/, String(bad));
assert.deepEqual({ ...userRequest({ username: "Ann", password: strong, role: "editor", tenant: hex, mustChange: true, disabled: false }) },
  { username: "ann", password: strong, role: "editor", tenant_id: lowerHex, must_change_password: true, disabled: false });
assert.deepEqual({ ...userRequest({ username: "root1", password: strong, role: "super_admin", tenant: "", mustChange: false, disabled: true }) },
  { username: "root1", password: strong, role: "super_admin", must_change_password: false, disabled: true });
assert.throws(() => userRequest({ username: "ann", password: "short", role: "viewer" }), /15 to 128/);
assert.throws(() => userRequest(null), /Choose a role/);
const account = { id: "a".repeat(32), username: "ann", role: "editor", tenant_id: null, disabled: false, must_change_password: true };
const same = { username: "", role: "editor", tenant: "", disabled: false, mustChange: true };
assert.throws(() => userChanges(undefined, same), /List users first/);
assert.throws(() => userChanges(account, same), /Nothing to change/);
assert.throws(() => userChanges(account, { ...same, username: "ANN" }), /Nothing to change/);
assert.deepEqual({ ...userChanges(account, { ...same, username: "bob.smith" }) }, { username: "bob.smith" });
assert.deepEqual({ ...userChanges(account, { ...same, role: "viewer", tenant: hex, disabled: true, mustChange: false }) },
  { role: "viewer", tenant_id: lowerHex, disabled: true, must_change_password: false });
assert.deepEqual({ ...userChanges({ ...account, tenant_id: lowerHex }, { ...same, role: "super_admin" }) }, { role: "super_admin", tenant_id: null });
assert.throws(() => userChanges(account, { ...same, role: "super_admin", tenant: hex }), /cannot belong to a tenant/);
assert.deepEqual({ ...userView({ ...account, role_label: "Editor", tenant_id: lowerHex, tenant_name: "Acme", disabled: true, created_unix_ms: 0 }) },
  { id: "a".repeat(32), username: "ann", role: "Editor", access: `tenant "Acme" (${lowerHex})`, status: "disabled (cannot sign in)", must_change_password: "yes, at next sign-in" });
assert.equal(userView({ id: "b", username: "root", role: "super_admin", tenant_id: null }).access, "whole server");
assert.equal(userView({ id: "c", username: "v", role: "viewer", tenant_id: null, must_change_password: false }).must_change_password, "no");
assert.equal(userView({ role: "auditor", role_label: "Auditor" }).role, "Auditor");
const users = userList({ items: [account], capacity: 1024 });
assert.deepEqual([users.count, users.limit, users.users[0].role, users.users[0].access], [1, 1024, "Editor", "all collections"]);
assert.throws(() => userList({}), /Unexpected user list/);

// Account graph knobs: sent only when the form supplies them (graph builds), never for super admins.
const graphUtils = require("../src/dashboard/graph.js").utils;
const graphFields = (graphs, access) => ({ graph: { graphs, access } });
assert.deepEqual({ ...userRequest({ username: "Ann", password: strong, role: "editor", tenant: "", ...graphFields("people, docs", "read") }, graphUtils) },
  { username: "ann", password: strong, role: "editor", must_change_password: true, disabled: false, graphs: ["people", "docs"], graph_access: "read" });
assert.deepEqual({ ...userRequest({ username: "Ann", password: strong, role: "viewer", tenant: "", ...graphFields("", "") }, graphUtils) },
  { username: "ann", password: strong, role: "viewer", must_change_password: true, disabled: false });
assert.deepEqual({ ...userRequest({ username: "root1", password: strong, role: "super_admin", tenant: "", ...graphFields("people", "read") }, graphUtils) },
  { username: "root1", password: strong, role: "super_admin", must_change_password: true, disabled: false });
assert.deepEqual({ ...userRequest({ username: "Ann", password: strong, role: "editor", tenant: "", ...graphFields("people", "") }) },
  { username: "ann", password: strong, role: "editor", must_change_password: true, disabled: false });
assert.throws(() => userRequest({ username: "Ann", password: strong, role: "viewer", tenant: "", ...graphFields("", "write") }, graphUtils), /Viewers cannot write graphs/);
assert.throws(() => userRequest({ username: "Ann", password: strong, role: "editor", tenant: hex, ...graphFields("x".repeat(29), "") }, graphUtils), /1 to 28/);
const graphAccount = { ...account, graphs: ["people"], graph_access: "read" };
assert.deepEqual({ ...userChanges(graphAccount, { ...same, username: "bob", ...graphFields("people", "read") }, graphUtils) }, { username: "bob" });
assert.throws(() => userChanges(graphAccount, { ...same, ...graphFields("people", "read") }, graphUtils), /Nothing to change/);
assert.deepEqual({ ...userChanges(graphAccount, { ...same, ...graphFields("[]", "") }, graphUtils) }, { graphs: [], graph_access: null });
assert.deepEqual({ ...userChanges(graphAccount, { ...same, role: "super_admin", ...graphFields("people", "read") }, graphUtils) },
  { role: "super_admin", graphs: null, graph_access: null });
assert.deepEqual({ ...userChanges(account, { ...same, role: "super_admin", ...graphFields("", "") }, graphUtils) }, { role: "super_admin" });
assert.equal(userView(graphAccount, graphUtils).graphs, "read: people");
assert.equal(userView({ ...account, graphs: null, graph_access: null }, graphUtils).graphs, "read and write: every graph");
assert.equal("graphs" in userView(account, graphUtils), false);
assert.equal(userList({ items: [graphAccount] }, graphUtils).users[0].graphs, "read: people");

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
assert.match(availability(null, false).lifecycle.reason, /Sign in first/);
// User management needs a super admin and a server with accounts enabled.
assert.equal(availability({ ...adminCaps, features: { ...adminCaps.features, accounts: true } }).users.allowed, true);
assert.match(availability(adminCaps).users.reason, /VLITE_ADMIN_DIR/);
assert.match(ro.users.reason, /requires a super admin/);
assert.match(availability(null, false).users.reason, /Sign in first/);
assert.match(explain(Object.assign(new Error("HTTP 409: cannot_modify_self"), { code: "cannot_modify_self", status: 409 })), /Use Change password/);
assert.match(explain(Object.assign(new Error("HTTP 422: weak_password"), { code: "weak_password", status: 422 })), /less predictable/);
assert.match(explain(Object.assign(new Error("HTTP 409: username_taken"), { code: "username_taken", status: 409 })), /ignore case/);
assert.match(explain(Object.assign(new Error("HTTP 403"), { code: "tenant_suspended", status: 403 })), /Resume the tenant first/);
assert.match(explain(Object.assign(new Error("HTTP 403"), { code: "tenant_unavailable", status: 403 })), /was deleted/);
assert.match(explain(Object.assign(new Error("HTTP 409: tenant_not_empty: x"), { code: "tenant_not_empty", status: 409 })), /Drop those collections and graphs first/);
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
    reads: [], readResponses: [],
    background: async (path) => {
      ctx.reads.push(path);
      const next = ctx.readResponses.shift();
      if (next instanceof Error) throw next;
      if (next === undefined) throw new Error("unexpected background read");
      return next;
    },
  };
  return ctx;
}
const reset = () => { for (const e of Object.values(elements)) { e.listeners.length = 0; e.inputs.length = 0; e.value = ""; e.textContent = ""; e.disabled = false; e.checked = false; e.hidden = false; e.attributes = {}; e.children = []; } };

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
  assert.match(elements["tenant-result"].textContent, /Drop those collections and graphs first/);
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

  // Users: List caches accounts; entering a listed ID loads it; Save sends only changed fields.
  ctx.caps = { ...adminCaps, features: { ...adminCaps.features, accounts: true } };
  tools.capabilities(ctx.caps);
  assert.equal(elements["user-result"].textContent, "");
  const uid = "a".repeat(32);
  const ann = { id: uid, username: "ann", role: "editor", role_label: "Editor", tenant_id: null, tenant_name: null, disabled: false, must_change_password: true };
  ctx.responses.push({ items: [ann], capacity: 1024 });
  await elements["user-list"].click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path, ctx.calls.at(-1).body], ["GET", "/v1/admin/users", undefined]);
  assert.match(elements["user-result"].textContent, /"count": 1/);
  assert.match(elements["user-result"].textContent, /"access": "all collections"/);
  elements["user-id"].value = ` ${uid.toUpperCase()} `;
  elements["user-id"].input();
  assert.deepEqual([elements["user-username"].value, elements["user-role"].value, elements["user-tenant"].value, elements["user-must-change"].checked, elements["user-disabled"].checked],
    ["ann", "editor", "", true, false]);
  assert.match(elements["user-result"].textContent, /"loaded"/);
  let userCalls = ctx.calls.length;
  await elements["user-save"].click();
  assert.equal(ctx.calls.length, userCalls);
  assert.match(elements["user-result"].textContent, /Nothing to change/);
  elements["user-role"].value = "viewer";
  elements["user-disabled"].checked = true;
  ctx.answer = false;
  await elements["user-save"].click();
  assert.equal(ctx.calls.length, userCalls);
  assert.equal(ctx.confirms.at(-1), `Update "ann" (${uid}): role → Viewer; account disabled → yes? Their open sessions end and they sign in again.`);
  ctx.answer = true;
  ctx.responses.push({ user: { ...ann, role: "viewer", role_label: "Viewer", disabled: true }, changed: ["role", "disabled"] });
  await elements["user-save"].click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path, { ...ctx.calls.at(-1).body }], ["PUT", `/v1/admin/users/${uid}`, { role: "viewer", disabled: true }]);
  assert.match(elements["user-result"].textContent, /"status": "disabled \(cannot sign in\)"/);
  await elements["user-save"].click();
  assert.match(elements["user-result"].textContent, /Nothing to change/);

  // Create validates locally, confirms, clears the password field and fills in the new ID.
  elements["user-id"].value = "";
  elements["user-username"].value = "Bob";
  elements["user-password"].value = "short";
  elements["user-role"].value = "viewer";
  elements["user-tenant"].value = "";
  elements["user-must-change"].checked = true;
  elements["user-disabled"].checked = false;
  userCalls = ctx.calls.length;
  await elements["user-create"].click();
  assert.match(elements["user-result"].textContent, /15 to 128 characters/);
  elements["user-password"].value = strong;
  elements["user-role"].value = "super_admin";
  elements["user-tenant"].value = hex;
  await elements["user-create"].click();
  assert.match(elements["user-result"].textContent, /cannot belong to a tenant/);
  assert.equal(ctx.calls.length, userCalls);
  elements["user-tenant"].value = "";
  const bobId = "b".repeat(32);
  ctx.responses.push({ user: { id: bobId, username: "bob", role: "super_admin", tenant_id: null, disabled: false, must_change_password: true } });
  await elements["user-create"].click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path, { ...ctx.calls.at(-1).body }],
    ["POST", "/v1/admin/users", { username: "bob", password: strong, role: "super_admin", must_change_password: true, disabled: false }]);
  assert.equal(ctx.confirms.at(-1), `Add super admin "bob" with access to the whole server? They must choose a new password at their first sign-in. Share the password with them privately.`);
  assert.equal(elements["user-password"].value, "");
  assert.equal(elements["user-id"].value, bobId);
  assert.match(elements["user-result"].textContent, /"added"/);

  // Set new password requires a password; the request carries the must-change choice.
  userCalls = ctx.calls.length;
  await elements["user-reset-password"].click();
  assert.equal(ctx.calls.length, userCalls);
  assert.match(elements["user-result"].textContent, /New password is required/);
  elements["user-password"].value = strong;
  elements["user-must-change"].checked = false;
  ctx.responses.push({ user: { id: bobId, username: "bob", role: "super_admin", tenant_id: null, disabled: false, must_change_password: false } });
  await elements["user-reset-password"].click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path, { ...ctx.calls.at(-1).body }], ["POST", `/v1/admin/users/${bobId}/password`, { password: strong, must_change_password: false }]);
  assert.equal(ctx.confirms.at(-1), `Set a new password for "bob" (${bobId})? Their open sessions end. Share the password with them privately.`);
  assert.equal(elements["user-password"].value, "");

  // Delete explains server refusals; a deleted account must be listed again before saving.
  ctx.responses.push(Object.assign(new Error("HTTP 409: cannot_modify_self"), { status: 409, code: "cannot_modify_self" }));
  await elements["user-delete"].click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path], ["DELETE", `/v1/admin/users/${bobId}`]);
  assert.match(ctx.confirms.at(-1), /cannot undo this\.$/);
  assert.match(elements["user-result"].textContent, /Use Change password/);
  ctx.responses.push({ deleted: true });
  await elements["user-delete"].click();
  assert.match(elements["user-result"].textContent, /"deleted": true/);
  userCalls = ctx.calls.length;
  await elements["user-save"].click();
  assert.equal(ctx.calls.length, userCalls);
  assert.match(elements["user-result"].textContent, /List users first/);

  // Disconnect forgets the typed password and the cached accounts.
  elements["user-password"].value = strong;
  tools.disconnect();
  assert.equal(elements["user-password"].value, "");
  assert.equal(elements["user-result"].textContent, "");
  elements["user-id"].value = uid;
  elements["user-username"].value = "";
  elements["user-id"].input();
  assert.equal(elements["user-username"].value, "");
  ctx.caps = adminCaps;
  tools.capabilities(adminCaps);
  assert.match(elements["user-result"].textContent, /User accounts are disabled/);

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
  assert.match(elements["user-result"].textContent, /User management requires a super admin/);
  assert.equal(elements["replication-result"].textContent, "");
  await elements["tenant-list"].click();
  await elements["tls-reload"].click();
  await elements["tenant-suspend"].click();
  await elements["user-list"].click();
  await elements["user-delete"].click();
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
  await compactionTests();
  console.log("admin tools tests passed");
})().catch((error) => { console.error(error); process.exit(1); });

// ---- File compaction: pure helpers, then the card controller against a fake backend. ----
async function compactionTests() {
  const { formatBytes, compactionForm, compactionSettings, compactionOutcome, compactionView, idempotencyKey } = utils;
  const MIB = 1048576;
  const flush = () => new Promise((resolve) => setImmediate(resolve));
  assert.equal(formatBytes(0), "0 bytes");
  assert.equal(formatBytes(1536), "1.5 KiB (1,536 bytes)");
  assert.equal(formatBytes(-1), "unknown");
  assert.equal(idempotencyKey((b) => b), "00000000-0000-4000-8000-000000000000");
  assert.match(idempotencyKey(), /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  const settings = { enabled: false, daily_start_utc: "02:00", window_minutes: 60, max_run_seconds: 900, min_file_bytes: 256 * MIB,
    min_growth_bytes: 128 * MIB, min_growth_percent: 25, cooldown_hours: 168, max_source_bytes: 1024 * MIB, history_policy: "unacknowledged" };
  // Round trip: what the form shows is exactly what Save sends back.
  assert.deepEqual({ ...compactionSettings(compactionForm(settings)).settings }, settings);
  assert.equal(compactionForm(settings).minFile, "256");
  const bad = compactionSettings({ ...compactionForm(settings), start: "25:00", window: "4", minFile: "10", maxSource: "2048", growthPercent: "5", cooldown: "1", enabled: true });
  assert.deepEqual(Object.keys(bad.errors).sort(), ["cooldown", "growthPercent", "history", "maxSource", "minFile", "start", "window"]);
  assert.equal(bad.settings, undefined);
  assert.match(compactionSettings({ ...compactionForm(settings), maxRun: "3600" }).errors.maxRun, /at most 3540 seconds/);
  assert.match(compactionSettings({ ...compactionForm(settings), minFile: "2000" }).errors.minFile, /largest file size/);
  // The test profile has no 64 MiB floor, so tiny qualification files can be scheduled.
  const tiny = compactionSettings({ ...compactionForm(settings), minFile: "0", minGrowth: "0.001", maxSource: "1" }, 0);
  assert.equal(tiny.settings.min_file_bytes, 0);
  assert.equal(tiny.settings.min_growth_bytes, 1049);
  assert.ok(compactionSettings({ ...compactionForm(settings), minFile: "0" }).errors.minFile);
  assert.equal(compactionSettings({ ...compactionForm(settings), enabled: true, history: true }).settings.history_policy, "resnapshot_allowed");
  assert.match(compactionOutcome({ outcome: "skipped", code: "no_material_savings" }), /kept using the original file.*not have been meaningfully smaller/);
  assert.match(compactionOutcome({ outcome: "failed", selection: "committed" }), /after switching/);

  const status = (over = {}) => ({ capability: true, profile: "linux_ext4", test_profile: false, available: true, reasons: [],
    file: { name: "data.vlite", bytes: 300 * MIB, sampled_unix_ms: 10 }, settings: { ...settings }, settings_version: 3, history_acknowledged: false,
    schedule: { enabled: false, suspended: false, failures: 0, baseline_bytes: null, growth_bytes: null, next_window_unix_ms: null, in_window: false, next_eligible_unix_ms: null },
    job: null, last_job: null, phase: null, fence: "open", recovery: { required: false, retained_bytes: 0, retained_copies: 0 }, selection: null, ...over });
  const job = (over = {}) => ({ id: "job-1", trigger: "manual", phase: "building", cancel_requested: false, created_unix_ms: 1, selection: "none", ...over });
  let view = compactionView(status());
  assert.equal(view.tone, "ready");
  assert.equal(view.canStart, true);
  assert.ok(view.facts.some(([k, v]) => k === "Last run" && v === "Never"));
  view = compactionView(status({ available: false, reasons: [{ code: "history_ack_required", message: "ack" }, { code: "unqualified_filesystem", message: "Not a qualified file system." }] }));
  assert.deepEqual(view.reasons, ["Not a qualified file system."]);
  assert.equal(view.canStart, false);
  view = compactionView(status({ available: false, job: job(), reasons: [{ code: "job_active", message: "busy" }] }));
  assert.equal(view.tone, "busy");
  assert.match(view.headline, /started from Compact now.*Building the compacted copy|In progress/);
  assert.deepEqual([view.canStart, view.canCancel, view.watch, view.reasons.length], [false, true, true, 0]);
  assert.equal(compactionView(status({ job: job({ phase: "selecting" }) })).canCancel, false);
  assert.equal(compactionView(status({ job: job({ cancel_requested: true }) })).canCancel, false);
  view = compactionView(status({ recovery: { required: true, code: "selection_unresolved", retained_bytes: 5, retained_copies: 1 } }));
  assert.equal(view.tone, "blocked");
  assert.equal(view.canStart, false);
  assert.ok(view.facts.some(([k]) => k === "Recovery copies kept"));
  assert.equal(compactionView(status({ fence: "maintenance" })).watch, true);
  assert.equal(compactionView({ capability: false, reasons: [{ code: "managed_runtime_required", message: "managed" }], settings: null, job: null }).headline, "Not available on this server.");
  const last = { id: "job-1", trigger: "manual", outcome: "succeeded", selection: "committed", before_bytes: 300 * MIB, after_bytes: 100 * MIB, saved_bytes: 200 * MIB, finished_unix_ms: 1 };
  assert.ok(compactionView(status({ last_job: last })).facts.some(([k, v]) => k === "Last size change" && /saved 200 MiB/.test(v)));
  assert.ok(compactionView(status({ settings: { ...settings, enabled: true }, schedule: { suspended: true, failures: 3 } })).facts.some(([, v]) => /Suspended after 3/.test(v)));

  // Controller wired to a fake backend.
  reset();
  timers.length = 0;
  const ctx = fakeCtx();
  const caps = { role: "admin", features: { compaction: true }, routes: [] };
  ctx.caps = caps;
  const tools = init(ctx);
  ctx.readResponses.push(status());
  tools.capabilities(caps);
  await flush();
  assert.deepEqual(ctx.reads, ["/v1/admin/compaction"]);
  assert.equal(ctx.calls.length, 0, "status loads through the background reader, not a foreground request");
  assert.match(elements["compaction-status"].textContent, /^Ready/);
  assert.equal(elements["compaction-status"].attributes["data-tone"], "ready");
  assert.equal(elements["compaction-window"].value, "60");
  assert.equal(elements["compaction-min-file"].value, "256");
  assert.match(elements["compaction-local-time"].textContent, /02:00 UTC is/);
  assert.equal(elements["compaction-facts"].children.length > 2, true);
  assert.equal(elements["compaction-reasons"].hidden, true);
  assert.deepEqual([elements["compaction-start"].disabled, elements["compaction-cancel"].disabled, elements["compaction-save"].disabled, elements["compaction-enabled"].disabled], [false, true, false, false]);
  assert.equal(timers.length, 0, "an idle card does not poll");

  // Compact now needs both acknowledgements on every start.
  await elements["compaction-start"].click();
  assert.match(ctx.errors.at(-1).message, /Tick both/);
  assert.equal(elements["compaction-ack-error"].textContent, "Tick both statements to continue.");
  assert.equal(elements["compaction-ack"].attributes["aria-invalid"], "true");
  assert.equal(ctx.calls.length, 0);
  elements["compaction-ack-downtime"].checked = elements["compaction-ack-history"].checked = true;
  ctx.answer = false;
  await elements["compaction-start"].click();
  assert.equal(ctx.calls.length, 0, "declining the confirmation sends nothing");
  assert.match(ctx.confirms.at(-1), /Reads and writes pause.*900 seconds.*new snapshot/);
  ctx.answer = true;
  // Unknown outcome (503): the retry reuses the same idempotency key.
  ctx.responses.push(Object.assign(new Error("HTTP 503"), { status: 503 }));
  await elements["compaction-start"].click();
  ctx.responses.push({ job: job({ phase: "preflight" }), replayed: true });
  ctx.readResponses.push(status({ available: false, job: job(), fence: "maintenance", reasons: [{ code: "job_active", message: "busy" }] }));
  await elements["compaction-start"].click();
  const [first, second] = ctx.calls;
  assert.deepEqual([first.method, first.path], ["POST", "/v1/admin/compaction/jobs"]);
  assert.equal(first.body.idempotency_key, second.body.idempotency_key);
  assert.deepEqual({ ...second.body, idempotency_key: "k" }, { expected_version: 3, acknowledge_downtime: true, acknowledge_history: true, idempotency_key: "k" });
  assert.equal(elements["compaction-ack-downtime"].checked || elements["compaction-ack-history"].checked, false, "acknowledgements reset after a start");
  assert.match(elements["compaction-result"].textContent, /"replayed": true/);
  assert.match(elements["compaction-status"].textContent, /In progress/);
  assert.deepEqual([elements["compaction-start"].disabled, elements["compaction-cancel"].disabled, elements["compaction-save"].disabled, elements["compaction-window"].disabled], [true, false, true, true]);
  assert.equal(timers.at(-1).ms, 5000);

  // A dropped poll while the file switches keeps watching instead of failing.
  ctx.readResponses.push(new Error("Failed to fetch"));
  let tick = timers.at(-1); await tick.fn();
  assert.match(elements["compaction-status"].textContent, /Waiting for the server to answer \(attempt 1\)/);
  assert.notEqual(timers.at(-1), tick);
  assert.equal(timers.at(-1).ms, 5000);
  ctx.readResponses.push(new Error("Failed to fetch"));
  tick = timers.at(-1); await tick.fn();
  assert.match(elements["compaction-status"].textContent, /attempt 2/);
  assert.equal(timers.at(-1).ms, 10000, "dropped polls back off toward 30 seconds");

  // Cancel goes to the running job and is refused once past the safe point.
  ctx.responses.push({ job: job({ cancel_requested: true }) });
  ctx.readResponses.push(status({ available: false, job: job({ cancel_requested: true }), fence: "maintenance" }));
  await elements["compaction-cancel"].click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path], ["POST", "/v1/admin/compaction/jobs/job-1/cancel"]);
  assert.match(ctx.confirms.at(-1), /original file stays in use/);
  assert.match(elements["compaction-status"].textContent, /Cancellation requested/);
  assert.equal(elements["compaction-cancel"].disabled, true);
  const callsBefore = ctx.calls.length;
  await elements["compaction-cancel"].click();
  assert.equal(ctx.calls.length, callsBefore);

  // Polling reports the finished run with before/after sizes and then stops.
  ctx.readResponses.push(status({ last_job: { ...last, outcome: "cancelled", code: "cancelled", selection: "none", after_bytes: null, saved_bytes: null } }));
  tick = timers.at(-1); timers.length = 0; await tick.fn();
  assert.match(elements["compaction-status"].textContent, /^Cancelled: the server kept using the original file\. Reason: an administrator cancelled it\. Ready/);
  assert.equal(timers.length, 0);
  ctx.readResponses.push(status({ last_job: last, schedule: { baseline_bytes: 100 * MIB, growth_bytes: 0 } }));
  await elements["compaction-refresh"].click();
  const facts = elements["compaction-facts"].children.map((row) => row.children.map((cell) => cell.textContent).join(": "));
  assert.ok(facts.some((f) => /^Last size change: 300 MiB .* before, 100 MiB .* after; saved 200 MiB/.test(f)), facts.join("\n"));

  // A later (scheduled) run replaced the watched job as the last job: the card reads the watched job by id.
  elements["compaction-ack-downtime"].checked = elements["compaction-ack-history"].checked = true;
  ctx.responses.push({ job: job({ id: "job-2", phase: "preflight" }), replayed: false });
  ctx.readResponses.push(status({ available: false, job: job({ id: "job-2" }), fence: "maintenance" }));
  await elements["compaction-start"].click();
  ctx.readResponses.push(status({ last_job: { ...last, id: "job-3", trigger: "scheduled" } }),
    { job: { ...last, id: "job-2", outcome: "skipped", code: "no_material_savings", selection: "none", after_bytes: null, saved_bytes: null } });
  tick = timers.at(-1); timers.length = 0; await tick.fn();
  assert.deepEqual(ctx.reads.slice(-2), ["/v1/admin/compaction", "/v1/admin/compaction/jobs/job-2"]);
  assert.match(elements["compaction-status"].textContent, /^Your earlier run: Skipped: the server kept using the original file\. Reason: the compacted file would not have been meaningfully smaller\. Ready/);
  assert.equal(timers.length, 0);

  // Save: inline errors stop the request and focus the first invalid field.
  const putsBefore = ctx.calls.length;
  elements["compaction-window"].value = "4";
  elements["compaction-window"].inputs.forEach((fn) => fn());
  elements["compaction-enabled"].checked = true;
  await elements["compaction-save"].click();
  assert.equal(ctx.calls.length, putsBefore);
  assert.match(elements["compaction-window-error"].textContent, /5 to 120/);
  assert.equal(elements["compaction-window"].attributes["aria-invalid"], "true");
  assert.match(elements["compaction-history-policy-error"].textContent, /needs this consent/);
  await new Promise((resolve) => setTimeout(resolve, 5));
  assert.equal(elements["compaction-window"].focused > 0, true);
  // A version conflict reloads the status without overwriting the edits.
  elements["compaction-window"].value = "90";
  elements["compaction-history-policy"].checked = true;
  ctx.responses.push(Object.assign(new Error("HTTP 409: settings_version_conflict"), { status: 409, code: "settings_version_conflict" }));
  ctx.readResponses.push(status({ settings_version: 4 }));
  await elements["compaction-save"].click();
  assert.match(elements["compaction-result"].textContent, /changed in another session/);
  assert.equal(elements["compaction-window"].value, "90", "edits survive a refresh");
  const enabled = { ...settings, enabled: true, window_minutes: 90, history_policy: "resnapshot_allowed" };
  ctx.responses.push(status({ settings: enabled, settings_version: 5, history_acknowledged: true }));
  await elements["compaction-save"].click();
  const put = ctx.calls.at(-1);
  assert.deepEqual([put.method, put.path, put.body.expected_version, put.body.acknowledge_history], ["PUT", "/v1/admin/compaction/config", 4, true]);
  assert.deepEqual({ ...put.body.settings }, enabled);
  assert.match(ctx.confirms.at(-1), /Turn on automatic compaction daily at 02:00 UTC for up to 90 minutes/);
  assert.match(elements["compaction-result"].textContent, /Saved\. Automatic compaction is on \(settings version 5\)/);
  assert.equal(elements["compaction-window-error"].hidden, true);

  // Gating: non-admins and servers without the feature get plain reasons and send nothing.
  const sent = ctx.calls.length, read = ctx.reads.length;
  ctx.caps = { role: "read_only", features: { compaction: true }, routes: [] };
  tools.capabilities(ctx.caps);
  assert.match(elements["compaction-result"].textContent, /File compaction requires a super admin/);
  assert.match(elements["compaction-start-help"].textContent, /requires a super admin/);
  assert.deepEqual([elements["compaction-start"].disabled, elements["compaction-refresh"].disabled, elements["compaction-save"].disabled], [true, true, true]);
  await elements["compaction-refresh"].click();
  ctx.caps = { role: "admin", features: {}, routes: [] };
  tools.capabilities(ctx.caps);
  assert.match(elements["compaction-result"].textContent, /does not offer file compaction/);
  assert.deepEqual([ctx.calls.length, ctx.reads.length], [sent, read]);
  // Disconnect stops any polling and locks the card.
  ctx.caps = caps;
  ctx.readResponses.push(status({ job: job(), available: false }));
  tools.capabilities(caps);
  await flush();
  const pollTimer = timers.at(-1);
  tools.disconnect();
  assert.equal(pollTimer.fn, null, "disconnect cancels the poll");
  assert.equal(elements["compaction-cancel"].disabled, true);
}