"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");

function element(id, tagName = "BUTTON") {
  const listeners = [];
  const inputs = [];
  const changes = [];
  return { id, tagName, value: "", textContent: "", disabled: false, checked: false, hidden: false, attributes: {}, children: [], focused: 0, listeners, inputs, changes,
    setAttribute(name, value) { this.attributes[name] = String(value); }, replaceChildren(...nodes) { this.children = nodes; },
    append(...nodes) { this.children.push(...nodes); }, focus() { this.focused++; },
    addEventListener(type, fn) { if (type === "click") listeners.push(fn); else if (type === "input") inputs.push(fn); else if (type === "change") changes.push(fn); },
    click() { return Promise.all(listeners.map((fn) => fn())); },
    input() { inputs.forEach((fn) => fn()); },
    change() { changes.forEach((fn) => fn()); } };
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
  "compaction-max-source-error", "compaction-save", "compaction-result",
  "cloud-backup-status", "cloud-backup-facts", "cloud-backup-reasons", "cloud-backup-refresh", "cloud-backup-run", "cloud-backup-collect", "cloud-backup-help",
  "cloud-backup-enabled", "cloud-backup-account", "cloud-backup-account-error", "cloud-backup-container", "cloud-backup-container-error",
  "cloud-backup-coordination", "cloud-backup-coordination-error", "cloud-backup-prefix", "cloud-backup-prefix-error",
  "cloud-backup-client-id", "cloud-backup-client-id-error", "cloud-backup-interval", "cloud-backup-interval-error",
  "cloud-backup-drill-every", "cloud-backup-drill-every-error", "cloud-backup-endpoint-note", "cloud-backup-retention",
  "cloud-backup-keep-sets", "cloud-backup-keep-sets-error", "cloud-backup-keep-days", "cloud-backup-keep-days-error",
  "cloud-backup-grace", "cloud-backup-grace-error", "cloud-backup-save", "cloud-backup-sets", "cloud-backup-set-id", "cloud-backup-set-facts",
  "cloud-backup-drill", "cloud-backup-restore", "cloud-backup-pin-reason", "cloud-backup-pin-reason-error", "cloud-backup-pin",
  "cloud-backup-pin-id", "cloud-backup-force", "cloud-backup-unpin", "cloud-backup-sets-help", "cloud-backup-result",
  "auto-failover-status", "auto-failover-facts", "auto-failover-refresh", "auto-failover-toggle", "auto-failover-help", "auto-failover-result"];
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
const reset = () => { for (const e of Object.values(elements)) { e.listeners.length = 0; e.inputs.length = 0; e.changes.length = 0; e.value = ""; e.textContent = ""; e.disabled = false; e.checked = false; e.hidden = false; e.attributes = {}; e.children = []; } };

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
  await cloudBackupTests();
  await autoFailoverTests();
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
async function cloudBackupTests() {
  const { cloudBackupForm, cloudBackupPolicy, cloudBackupView, cloudBackupSets, cloudBackupSetFacts, cloudRunOutcome, cloudRestoreOutcome, cloudInterval, cloudCollectionText, cloudDrillText } = utils;
  // Values built inside the sandbox have its prototypes; compare them as JSON.
  const plain = (value) => JSON.parse(JSON.stringify(value));
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const T = Date.UTC(2026, 10, 14, 12, 30), WHEN = "2026-11-14 12:30 UTC";

  assert.deepEqual([60, 450, 3600, 172800, 0, NaN].map((s) => cloudInterval(s)), ["1 minute", "7.5 minutes", "1 hour", "2 days", "unknown", "unknown"]);
  assert.deepEqual({ ...cloudBackupForm(null) }, { enabled: false, account: "", container: "", coordination: "", prefix: "", clientId: "", interval: "60",
    drillEvery: "24", retention: false, keepSets: "3", keepDays: "30", grace: "24" });
  // Round trip: what the form shows is exactly what Save sends back.
  const policy = { target: { account: "vlitebackups", container: "backups", coordination_container: "backups-lease", prefix: "vlite/prod/", auth: "managed_identity",
    client_id: "01234567-89ab-cdef-0123-456789abcdef" }, enabled: true, interval_secs: 450, retention_enabled: true, keep_sets: 5, keep_days: 45, orphan_grace_hours: 12, drill_every: 6 };
  const form = cloudBackupForm(policy);
  assert.equal(form.interval, "7.5");
  assert.deepEqual(plain(cloudBackupPolicy(form, policy)), { errors: {}, policy });
  const bare = cloudBackupPolicy({ ...form, coordination: "", clientId: " ", enabled: false, retention: false }, null).policy;
  assert.deepEqual(plain(bare.target), { account: "vlitebackups", container: "backups", prefix: "vlite/prod/", auth: "managed_identity" });
  assert.deepEqual([bare.enabled, bare.retention_enabled], [false, false]);
  assert.equal(cloudBackupPolicy({ ...form, interval: "10080" }, policy).policy.interval_secs, 604800);
  // Saving from the page keeps archive settings it cannot show.
  const archived = { ...policy, archive: { enabled: true, interval_secs: 600 } };
  assert.deepEqual(plain(cloudBackupPolicy(cloudBackupForm(archived), archived)), { errors: {}, policy: archived });
  assert.equal("archive" in cloudBackupPolicy(form, policy).policy, false);
  // Every field is checked at once, in plain words, and nothing is built.
  const bad = cloudBackupPolicy({ account: "Bad_Account", container: "-x", coordination: "", prefix: "/abs/", clientId: "nope", interval: "4",
    drillEvery: "101", keepSets: "2", keepDays: "29", grace: "0" }, null);
  assert.equal(bad.policy, undefined);
  assert.deepEqual(Object.keys(bad.errors).sort(), ["account", "clientId", "container", "drillEvery", "grace", "interval", "keepDays", "keepSets", "prefix"]);
  assert.match(cloudBackupPolicy({ ...form, coordination: "backups" }, policy).errors.coordination, /different container/);
  for (const prefix of ["vlite", "vlite//", "vlite/../", "./", "UPPER/"]) assert.ok(cloudBackupPolicy({ ...form, prefix }, policy).errors.prefix, prefix);
  assert.ok(cloudBackupPolicy({ ...form, interval: "5.12345" }, policy).errors.interval);
  // A policy signed in with an emulator key is left to the REST API.
  const emulator = { ...policy, target: { ...policy.target, auth: "emulator_shared_key", endpoint: "http://127.0.0.1:10000/devstoreaccount1" } };
  assert.match(cloudBackupPolicy(form, emulator).errors.account, /test emulator key/);

  assert.equal(cloudDrillText(null), "Never");
  assert.equal(cloudDrillText({ ok: true, unix_ms: T }), `Passed, ${WHEN}`);
  assert.equal(cloudDrillText({ ok: false, unix_ms: T, error: { code: "cloud_set_corrupt", message: "checksum mismatch" } }), `Failed, ${WHEN}: checksum mismatch`);
  assert.equal(cloudDrillText({ ok: false, unix_ms: T, error: { code: "cloud_set_corrupt" } }), `Failed, ${WHEN}: cloud_set_corrupt`);
  assert.equal(cloudDrillText({ ok: false, unix_ms: T, detail: "object 3 missing" }), `Failed, ${WHEN}: object 3 missing`);
  assert.equal(cloudDrillText({ ok: false }), "Failed, --: no reason recorded");
  assert.equal(cloudCollectionText(undefined), "Never");
  assert.equal(cloudCollectionText({ ok: false, unix_ms: T, error: { message: "403 from storage" } }), `Failed, ${WHEN}: 403 from storage`);
  assert.equal(cloudCollectionText({ ok: true, unix_ms: T, retention_enabled: true, result: { deleted: ["a", "b"], orphans_deleted: { count: 1 }, failures: [] } }),
    `${WHEN}: removed 2 old and 1 unfinished backups`);
  assert.equal(cloudCollectionText({ ok: true, unix_ms: T, result: { failures: { count: 2 } } }),
    `${WHEN}: removed 0 old and 0 unfinished backups (automatic deletion of old backups is off); 2 could not be removed`);

  const state = (over = {}, statusOver = {}) => ({ configured: true, policy, policy_uncertain: false, installation_id: "inst-1", running: false, next_run_unix_ms: T + 60000,
    status: { runs: 3, published: 2, failures: 1, skipped_busy: 0, last_success_unix_ms: T, last_set: "set-2", ...statusOver }, ...over });
  assert.equal(cloudBackupView(null).headline, "Cloud backup status not loaded.");
  const ready = cloudBackupView(state());
  assert.deepEqual([ready.headline, ready.tone, ready.canRun], ["On: a backup is made every 7.5 minutes.", "ready", true]);
  assert.deepEqual(plain(ready.facts), [
    ["Storage", "Account vlitebackups, container backups, coordination container backups-lease, folder vlite/prod/"],
    ["Automatic backups", "On, every 7.5 minutes"],
    ["Next automatic backup", "2026-11-14 12:31 UTC"],
    ["Old backups", "Deleted automatically, keeping the newest 5 and any younger than 45 days"],
    ["This server's ID", "inst-1"],
    ["Last successful backup", `${WHEN}, backup set-2`],
    ["Backups so far", "2 made, 1 failed, 0 skipped because the server was busy"],
    ["Last restore test", "Never"],
    ["Last clean-up", "Never"]]);
  assert.deepEqual(plain(ready.reasons), []);
  const uncertain = cloudBackupView(state({ policy_uncertain: true, running: true }));
  assert.match(uncertain.headline, /could not be confirmed/);
  assert.deepEqual([uncertain.tone, uncertain.canRun], ["blocked", false]);
  assert.match(uncertain.reasons[0], /could not confirm which settings are saved/);
  const unset = cloudBackupView({ configured: false, policy: null, policy_uncertain: false, installation_id: "inst-1", status: {} });
  assert.match(unset.headline, /^Not set up yet/);
  assert.deepEqual([unset.tone, unset.canRun, plain(unset.facts)], ["idle", false, [["This server's ID", "inst-1"]]]);
  const busy = cloudBackupView(state({}, { job: { kind: "run" } }));
  assert.deepEqual([busy.headline, busy.tone, busy.canRun], ["A backup or clean-up is running now.", "busy", false]);
  const failed = cloudBackupView(state({}, { last_failure_unix_ms: T + 1, last_error: { code: "cloud_store_failed", message: "403" }, interrupted: 1, pending: { set_id: "set-3" },
    last_drill: { ok: false, unix_ms: T, error: { message: "bad" } }, last_collection: { ok: false, unix_ms: T, error: { code: "x" } }, clock_reset_unix_ms: T }));
  assert.deepEqual([failed.headline, failed.tone, failed.canRun], ["The latest backup failed. See what needs attention below.", "blocked", true]);
  assert.deepEqual(plain(failed.reasons), [
    "The latest backup failed (cloud_store_failed): 403",
    "1 earlier run(s) did not finish because the server stopped. Clean up now removes what they left behind.",
    "A backup upload (set-3) has not been confirmed as complete yet.",
    "The latest restore test failed: bad",
    "The latest clean-up failed: x",
    `The server clock moved backwards at ${WHEN}, so scheduled times were reset to the clock.`]);
  assert.equal(cloudBackupView(state({}, { last_failure_unix_ms: T - 1 })).tone, "ready", "a failure before the latest success is history");
  const off = cloudBackupView(state({ policy: { ...policy, enabled: false, retention_enabled: false } }));
  assert.deepEqual([off.headline, off.tone], ["Set up. Automatic backups are off; Back up now still works.", "idle"]);
  assert.ok(!off.facts.some(([term]) => term === "Next automatic backup"));
  assert.deepEqual(plain(off.facts.find(([term]) => term === "Old backups")), ["Old backups", "Never deleted automatically"]);

  assert.equal(cloudRunOutcome({ set_id: "set-4", drill: { ok: true, unix_ms: T }, collection: { ok: true, unix_ms: T, retention_enabled: true, result: {} } }),
    `Backup set-4 was uploaded. Restore test: Passed, ${WHEN}. Clean-up: ${WHEN}: removed 0 old and 0 unfinished backups.`);
  assert.equal(cloudRunOutcome({ set_id: "set-4", drill: { ok: true }, stopped_for_shutdown: true }),
    "Backup set-4 was uploaded. The server began shutting down, so the restore test and clean-up were skipped.");
  assert.equal(cloudRunOutcome(null), "The backup finished.");
  assert.match(cloudRestoreOutcome({ name: "restore-set-4.vlite", set_id: "set-4", bundle: { credentials: 2 }, pin_released: false }),
    /^Backup set-4 was restored as a new copy named restore-set-4\.vlite .*The live database was not changed\. 2 credentials must be issued again .*could not be released.*To use the copy: open Backups and restore above/);
  assert.match(cloudRestoreOutcome({ set_id: "s", name: "c", bundle: { credentials: 1 } }), /1 credential must be issued again/);
  assert.doesNotMatch(cloudRestoreOutcome({ name: "c", set_id: "s", bundle: { credentials: 0 }, pin_released: true }), /credential|could not be released/);

  const record = (over = {}) => ({ state: "published", created_unix_ms: T, verified_unix_ms: T, last_drill: { ok: true, unix_ms: T, detail: "" }, pins: [], ...over });
  const setsValue = { installation_id: "inst-1", tombstoned: ["set-0"], sets: [
    { set_id: "set-1", data_objects: 4, record: record({ pins: [{ id: "p1", reason: "audit", created_unix_ms: T, kind: "operator" }, { id: "p2", reason: "restore", created_unix_ms: T, kind: "operation" }] }) },
    { set_id: "set-3", data_objects: 1, record: record({ state: "staging", last_drill: undefined }) },
    { set_id: "set-2", error: { code: "cloud_set_corrupt", message: "record unreadable" } },
    { record: record() }] };
  const sets = cloudBackupSets(setsValue);
  assert.equal(sets.removing, 1);
  assert.deepEqual(plain(sets.items.map((i) => [i.id, i.state, i.complete, i.label])), [
    ["set-3", "unfinished", false, `${WHEN}, unfinished (set-3)`],
    ["set-2", "unreadable", false, "unknown time, unreadable (set-2)"],
    ["set-1", "complete", true, `${WHEN}, complete, held (set-1)`]]);
  assert.deepEqual(plain(sets.items[2].pins), [{ id: "p1", label: "Held by an operator: audit" }, { id: "p2", label: "Held by a running operation: restore" }]);
  assert.deepEqual(plain(cloudBackupSetFacts(sets.items[2])), [["Backup ID", "set-1"], ["State", "Complete"], ["Created", WHEN], ["Last verified", WHEN],
    ["Restore test", `Passed, ${WHEN}`], ["Holds", "2"], ["Stored files", "4"]]);
  assert.deepEqual(plain(cloudBackupSetFacts(sets.items[1])), [["Backup ID", "set-2"], ["State", "Could not be read"], ["Problem", "record unreadable"]]);
  assert.deepEqual(plain(cloudBackupSetFacts(null)), []);

  // Wiring: nothing is fetched until Refresh status.
  reset(); timers.length = 0;
  const ctx = fakeCtx();
  const caps = { role: "admin", features: { cloud_backup: true }, routes: [] };
  ctx.caps = caps;
  const tools = init(ctx);
  tools.capabilities(caps);
  const el = (id) => elements[`cloud-backup-${id}`];
  const disabled = (...names) => names.map((name) => el(name).disabled);
  assert.equal(ctx.calls.length, 0);
  assert.equal(el("status").textContent, "Cloud backup status not loaded.");
  assert.equal(el("help").textContent, "Select Refresh status first.");
  assert.match(el("sets-help").textContent, /^Select Load stored backups/);
  assert.deepEqual(disabled("refresh", "sets", "save", "run", "collect", "drill"), [false, false, false, true, true, true]);
  await el("save").click();
  assert.equal(el("result").textContent, "Select Refresh status first.");
  assert.equal(ctx.calls.length, 0);

  ctx.responses.push(state());
  await el("refresh").click();
  assert.deepEqual(plain(ctx.calls.at(-1)), { method: "GET", path: "/v1/admin/cloud-backup", options: { data: false } });
  assert.equal(el("result").textContent, "Status refreshed.");
  assert.equal(el("status").textContent, "On: a backup is made every 7.5 minutes.");
  assert.equal(el("status").attributes["data-tone"], "ready");
  assert.equal(el("facts").children.length, 9);
  assert.deepEqual(el("facts").children[0].children.map((node) => node.textContent), plain(ready.facts[0]));
  assert.deepEqual([el("account").value, el("interval").value, el("enabled").checked, el("retention").checked], ["vlitebackups", "7.5", true, true]);
  assert.deepEqual(disabled("run", "collect"), [false, false]);
  assert.match(el("help").textContent, /^Back up now uploads/);
  assert.equal(el("endpoint-note").hidden, true);

  // Invalid settings: every bad field is marked, the first is focused, and nothing is sent.
  const focused = el("account").focused;
  el("account").value = "Bad"; el("account").input();
  el("keep-sets").value = "2"; el("keep-sets").input();
  await el("save").click();
  await wait(5);
  assert.equal(el("result").textContent, "Fix the highlighted settings; nothing was saved.");
  assert.equal(ctx.calls.length, 1);
  assert.deepEqual([el("account").attributes["aria-invalid"], el("account-error").hidden, el("keep-sets-error").textContent, el("container").attributes["aria-invalid"]],
    ["true", false, "Enter a whole number from 3 to 1000.", "false"]);
  assert.equal(el("account").focused, focused + 1);
  el("account").value = "vlitebackups"; el("account").input();
  assert.deepEqual([el("account").attributes["aria-invalid"], el("account-error").hidden], ["false", true]);
  el("keep-sets").value = "7"; el("keep-sets").input();
  el("enabled").checked = false; el("enabled").input();
  ctx.answer = false;
  await el("save").click();
  assert.equal(ctx.calls.length, 1, "a declined save sends nothing");
  assert.match(ctx.confirms.at(-1), /^Save cloud backup settings \(account vlitebackups, container backups, folder vlite\/prod\/\)\? Automatic backups are off; nothing is uploaded until you select Back up now\. Old backups are deleted automatically, keeping the newest 7 and any younger than 45 days\./);
  ctx.answer = true;
  const saved = { ...policy, enabled: false, keep_sets: 7 };
  ctx.responses.push(state({ policy: saved }));
  await el("save").click();
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path], ["PUT", "/v1/admin/cloud-backup"]);
  assert.deepEqual(plain(ctx.calls.at(-1).body), saved);
  assert.equal(el("result").textContent, "Saved. Automatic backups are off.");
  assert.equal(el("status").textContent, "Set up. Automatic backups are off; Back up now still works.");

  // Back up now reports the outcome and reloads the status, keeping unsaved edits.
  el("prefix").value = "vlite/next/"; el("prefix").input();
  ctx.responses.push({ set_id: "set-4", drill: { ok: true, unix_ms: T }, collection: { ok: true, unix_ms: T, retention_enabled: true, result: {} } }, state({ policy: saved }));
  await el("run").click();
  assert.match(ctx.confirms.at(-1), /^Back up to account vlitebackups now\?/);
  assert.deepEqual(ctx.calls.slice(-2).map((c) => [c.method, c.path]), [["POST", "/v1/admin/cloud-backup/run"], ["GET", "/v1/admin/cloud-backup"]]);
  assert.equal(el("result").textContent, `Backup set-4 was uploaded. Restore test: Passed, ${WHEN}. Clean-up: ${WHEN}: removed 0 old and 0 unfinished backups.`);
  assert.equal(el("prefix").value, "vlite/next/", "a status reload keeps unsaved edits");
  ctx.responses.push({ ok: true, unix_ms: T, retention_enabled: true, result: { deleted: ["set-0"] } }, state({ policy: saved }));
  await el("collect").click();
  assert.match(ctx.confirms.at(-1), /^Clean up now\? This removes unfinished uploads older than 12 hours and old backups beyond the newest 7 that are older than 45 days\./);
  assert.deepEqual(ctx.calls.slice(-2).map((c) => [c.method, c.path]), [["POST", "/v1/admin/cloud-backup/collect"], ["GET", "/v1/admin/cloud-backup"]]);
  assert.equal(el("result").textContent, `Clean-up ${WHEN}: removed 1 old and 0 unfinished backups`);
  ctx.responses.push(state({ policy: saved }));
  await el("refresh").click();
  assert.equal(el("prefix").value, "vlite/prod/", "Refresh status discards unsaved edits");

  // Stored backups: newest first; unfinished ones cannot be tested, restored or held.
  ctx.responses.push(setsValue);
  await el("sets").click();
  assert.equal(ctx.calls.at(-1).path, "/v1/admin/cloud-backup/sets");
  assert.equal(el("result").textContent, "3 stored backups found, newest first. 1 more is being removed.");
  assert.equal(el("set-id").value, "set-3");
  assert.deepEqual(el("set-id").children.map((o) => o.value), ["set-3", "set-2", "set-1"]);
  assert.deepEqual(disabled("drill", "restore", "pin", "unpin"), [true, true, true, true]);
  assert.match(el("sets-help").textContent, /not complete/);
  await el("drill").click();
  assert.equal(el("result").textContent, "Choose a complete backup first.");
  el("set-id").value = "set-1"; el("set-id").change();
  assert.equal(el("pin-id").value, "p1");
  assert.deepEqual(el("pin-id").children.map((o) => o.textContent), ["Held by an operator: audit", "Held by a running operation: restore"]);
  assert.deepEqual(disabled("drill", "restore", "pin", "unpin"), [false, false, false, false]);
  assert.equal(el("set-facts").children.length, 7);

  ctx.responses.push({ set_id: "set-1", drill: { ok: false, unix_ms: T, detail: "object 2 missing" } }, setsValue);
  await el("drill").click();
  assert.match(ctx.confirms.at(-1), /^Test backup set-1\?/);
  assert.deepEqual(ctx.calls.slice(-2).map((c) => [c.method, c.path]), [["POST", "/v1/admin/cloud-backup/sets/set-1/drill"], ["GET", "/v1/admin/cloud-backup/sets"]]);
  assert.equal(el("result").textContent, `Restore test of set-1: Failed, ${WHEN}: object 2 missing. 3 stored backups found, newest first. 1 more is being removed.`);
  assert.equal(el("set-id").value, "set-1", "a reload keeps the selection");
  const beforeRestore = ctx.calls.length;
  ctx.responses.push({ name: "restore-1", set_id: "set-1", bundle: { credentials: 0 }, pin_released: true });
  await el("restore").click();
  assert.equal(ctx.calls.length, beforeRestore + 1, "restore does not reload");
  assert.deepEqual([ctx.calls.at(-1).method, ctx.calls.at(-1).path], ["POST", "/v1/admin/cloud-backup/sets/set-1/restore"]);
  assert.match(el("result").textContent, /^Backup set-1 was restored as a new copy named restore-1 /);

  // Holds: the reason is checked before anything is sent; holding needs no confirmation.
  const beforePin = ctx.calls.length, confirms = ctx.confirms.length;
  for (const [reason, expected] of [["  ", /^Enter a reason\. Nothing was held\.$/], ["\u00e9".repeat(101), /^Use at most 200 characters .* Nothing was held\.$/],
    ["a\nb", /^Remove line breaks and other control characters\. Nothing was held\.$/]]) {
    el("pin-reason").value = reason;
    await el("pin").click();
    assert.match(el("result").textContent, expected);
    assert.equal(el("pin-reason-error").hidden, false);
  }
  assert.equal(ctx.calls.length, beforePin);
  el("pin-reason").value = "keep for audit"; el("pin-reason").input();
  assert.equal(el("pin-reason-error").hidden, true);
  ctx.responses.push({ set_id: "set-1", pin: { id: "p3", reason: "keep for audit", kind: "operator" } }, setsValue);
  await el("pin").click();
  assert.deepEqual(plain(ctx.calls.at(-2)), { method: "POST", path: "/v1/admin/cloud-backup/sets/set-1/pins", body: { reason: "keep for audit" }, options: { data: false } });
  assert.equal(ctx.confirms.length, confirms);
  assert.equal(el("pin-reason").value, "");
  assert.match(el("result").textContent, /^Backup set-1 is held \(hold p3\); clean-up will not remove it until the hold is released\. 3 stored backups found/);
  el("pin-id").value = "p2"; el("pin-id").change();
  el("force").checked = true;
  ctx.responses.push(undefined, setsValue);
  await el("unpin").click();
  assert.match(ctx.confirms.at(-1), /^Release hold p2 on backup set-1\?.*You confirmed the operation that made this hold is no longer running\.$/);
  assert.deepEqual([ctx.calls.at(-2).method, ctx.calls.at(-2).path], ["DELETE", "/v1/admin/cloud-backup/sets/set-1/pins/p2?force=true"]);
  assert.equal(el("force").checked, false);
  assert.match(el("result").textContent, /^Hold p2 released\. 3 stored backups found/);

  // Server refusals show the message plus a plain hint.
  ctx.responses.push(Object.assign(new Error("HTTP 409: cloud_backup_busy"), { code: "cloud_backup_busy", status: 409 }));
  await el("run").click();
  assert.equal(el("result").textContent, "HTTP 409: cloud_backup_busy\nAnother cloud backup operation is running. Wait for it to finish, then select Refresh status.");
  assert.equal(ctx.errors.at(-1).code, "cloud_backup_busy");

  // Emulator sign-in is shown but not editable; a managed-identity endpoint is just shown.
  ctx.responses.push(state({ policy: emulator }));
  await el("refresh").click();
  assert.match(el("endpoint-note").textContent, /local test emulator/);
  assert.equal(el("endpoint-note").hidden, false);
  assert.deepEqual(disabled("save", "account", "enabled", "run"), [true, true, true, false]);
  ctx.responses.push(state({ policy: { ...policy, target: { ...policy.target, endpoint: "https://example.blob.core.windows.net" } } }));
  await el("refresh").click();
  assert.equal(el("endpoint-note").textContent, "Storage address: https://example.blob.core.windows.net.");
  assert.deepEqual(disabled("save", "account"), [false, false]);
  // While a backup runs, settings and runs wait.
  ctx.responses.push(state({ running: true }));
  await el("refresh").click();
  assert.deepEqual(disabled("save", "run", "collect", "drill", "restore", "refresh", "sets"), [true, true, true, true, true, false, false]);
  assert.match(el("help").textContent, /^A backup or clean-up is running/);
  assert.match(el("sets-help").textContent, /^Test and Restore are off/);

  // Gating: non-admins and servers without the feature get plain reasons and send nothing.
  const sent = ctx.calls.length;
  ctx.caps = { role: "read_only", features: { cloud_backup: true }, routes: [] };
  tools.capabilities(ctx.caps);
  assert.match(el("result").textContent, /^Cloud backup requires a super admin/);
  assert.match(el("help").textContent, /requires a super admin/);
  assert.deepEqual(disabled("refresh", "sets", "save", "run"), [true, true, true, true]);
  await el("refresh").click();
  ctx.caps = { role: "admin", features: {}, routes: [] };
  tools.capabilities(ctx.caps);
  assert.match(el("result").textContent, /does not offer cloud backup/);
  await el("sets").click();
  assert.equal(ctx.calls.length, sent);

  // Disconnect clears the card and locks it; late results are dropped.
  ctx.caps = caps;
  tools.capabilities(caps);
  ctx.responses.push(state());
  await el("refresh").click();
  tools.disconnect();
  assert.equal(el("status").textContent, "Cloud backup status not loaded.");
  assert.deepEqual([el("account").value, el("interval").value, el("result").textContent, el("facts").hidden], ["", "60", "", true]);
  assert.deepEqual(disabled("refresh", "save", "sets"), [true, true, true]);
  assert.equal(el("help").textContent, "Sign in first.");
  tools.capabilities(caps);
  ctx.responses.push(() => { tools.disconnect(); return state(); });
  await el("refresh").click();
  assert.equal(el("status").textContent, "Cloud backup status not loaded.");
}

async function autoFailoverTests() {
  const { autoFailoverView, autoFailoverRequest } = utils;
  const plain = (value) => JSON.parse(JSON.stringify(value));
  const T = Date.UTC(2026, 10, 14, 12, 30), WHEN = "2026-11-14 12:30 UTC";
  const base = { available: true, enabled: false, revision: null, updated_unix_ms: null, source: null, invalid: false, qualified: false, gate: "GATE-005", warning: "w" };
  const on = { ...base, enabled: true, revision: "r2", updated_unix_ms: T, source: "rest:admin" };

  // View: "on" is never shown as healthy, and an unavailable server cannot be switched.
  assert.deepEqual(plain(autoFailoverView(null)), { headline: "Automatic failover status not loaded.", tone: "idle", facts: [], available: false, enabled: false, invalid: false, canToggle: false });
  const off = autoFailoverView(base);
  assert.match(off.headline, /^Off: failover happens only when a person runs it/);
  assert.deepEqual([off.tone, off.enabled, off.canToggle], ["idle", false, true]);
  assert.deepEqual(plain(off.facts), [["Automatic failover", "Off"], ["Proven safe", "No: GATE-005 has not passed"]]);
  const enabled = autoFailoverView(on);
  assert.match(enabled.headline, /^On \(not yet proven safe\)/);
  assert.match(enabled.headline, /recent changes can be lost/);
  assert.deepEqual([enabled.tone, enabled.enabled], ["blocked", true]);
  assert.deepEqual(plain(enabled.facts), [["Automatic failover", "On"], ["Proven safe", "No: GATE-005 has not passed"], ["Last changed", WHEN],
    ["Changed by", "rest:admin"], ["Setting version", "r2"]]);
  const damaged = autoFailoverView({ ...base, invalid: true });
  assert.match(damaged.headline, /damaged/);
  assert.deepEqual([damaged.tone, damaged.invalid, damaged.canToggle], ["blocked", true, true]);
  const missing = autoFailoverView({ ...base, available: false, enabled: true, invalid: true });
  assert.match(missing.headline, /^Not set up/);
  assert.deepEqual([missing.enabled, missing.invalid, missing.canToggle, missing.facts.length], [false, false, false, 0]);

  // Request: turning on acknowledges the warning and carries the revision it saw; turning off always wins.
  assert.deepEqual(plain(autoFailoverRequest(base, true)), { enabled: true, acknowledge_unqualified: true });
  assert.deepEqual(plain(autoFailoverRequest({ ...base, revision: "r1" }, true)), { enabled: true, acknowledge_unqualified: true, expected_revision: "r1" });
  assert.deepEqual(plain(autoFailoverRequest(on, false)), { enabled: false });
  assert.match(explain(Object.assign(new Error("HTTP 409"), { code: "auto_failover_not_configured", status: 409 })), /--authority-target-file/);
  assert.match(explain(Object.assign(new Error("HTTP 409"), { code: "auto_failover_conflict", status: 409 })), /Select Refresh status/);
  assert.match(explain(Object.assign(new Error("HTTP 409"), { code: "auto_failover_invalid", status: 409 })), /Turn automatic failover off to replace it/);

  // Wiring: nothing is fetched until Refresh status; the toggle stays off until a status is loaded.
  reset(); timers.length = 0;
  const ctx = fakeCtx();
  const caps = { role: "admin", features: {}, routes: [] };
  ctx.caps = caps;
  const tools = init(ctx);
  tools.capabilities(caps);
  const el = (id) => elements[`auto-failover-${id}`];
  const disabled = (...names) => names.map((name) => el(name).disabled);
  assert.equal(ctx.calls.length, 0);
  assert.equal(el("status").textContent, "Automatic failover status not loaded.");
  assert.equal(el("help").textContent, "Select Refresh status first.");
  assert.deepEqual(disabled("refresh", "toggle"), [false, true]);
  await el("toggle").click();
  assert.equal(el("result").textContent, "Select Refresh status first.");
  assert.equal(ctx.calls.length, 0);

  ctx.responses.push(base);
  await el("refresh").click();
  assert.deepEqual(plain(ctx.calls.at(-1)), { method: "GET", path: "/v1/admin/auto-failover", options: { data: false } });
  assert.equal(el("result").textContent, "Status refreshed.");
  assert.equal(el("status").attributes["data-tone"], "idle");
  assert.equal(el("facts").children.length, 2);
  assert.deepEqual([el("toggle").disabled, el("toggle").textContent], [false, "Turn on automatic failover"]);
  assert.match(el("help").textContent, /does not start a supervisor/);

  // Declining the warning sends nothing.
  ctx.answer = false;
  await el("toggle").click();
  assert.match(ctx.confirms.at(-1), /NOT yet proven safe \(GATE-005 has not passed\)/);
  assert.match(ctx.confirms.at(-1), /can be lost/);
  assert.equal(ctx.calls.length, 1);
  // Turning on: acknowledged PUT, then the card shows the stored setting.
  ctx.answer = true;
  ctx.responses.push(on);
  await el("toggle").click();
  assert.deepEqual(plain(ctx.calls.at(-1)), { method: "PUT", path: "/v1/admin/auto-failover", body: { enabled: true, acknowledge_unqualified: true }, options: { data: false } });
  assert.equal(el("result").textContent, "Automatic failover is on (not yet proven safe).");
  assert.equal(el("status").attributes["data-tone"], "blocked");
  assert.deepEqual([el("toggle").textContent, el("facts").children.length], ["Turn off automatic failover", 5]);
  assert.match(el("help").textContent, /stops every supervisor/);
  // Turning off: plain PUT.
  ctx.responses.push({ ...base, revision: "r3", updated_unix_ms: T, source: "rest:admin" });
  await el("toggle").click();
  assert.match(ctx.confirms.at(-1), /^Turn off automatic failover\?/);
  assert.deepEqual(plain(ctx.calls.at(-1).body), { enabled: false });
  assert.equal(el("result").textContent, "Automatic failover is off.");
  assert.equal(el("toggle").textContent, "Turn on automatic failover");
  // Turning on again carries the revision it saw; a conflict is explained and the stale status is dropped.
  ctx.responses.push(Object.assign(new Error("HTTP 409: auto_failover_conflict"), { code: "auto_failover_conflict", status: 409 }));
  await el("toggle").click();
  assert.deepEqual(plain(ctx.calls.at(-1).body), { enabled: true, acknowledge_unqualified: true, expected_revision: "r3" });
  assert.match(el("result").textContent, /Someone else changed automatic failover first/);
  assert.equal(el("status").textContent, "Automatic failover status not loaded.");
  assert.deepEqual([el("toggle").disabled, el("help").textContent], [true, "Select Refresh status first."]);
  // A damaged setting offers only Turn off.
  ctx.responses.push({ ...base, invalid: true });
  await el("refresh").click();
  assert.equal(el("toggle").textContent, "Turn off automatic failover");
  ctx.responses.push(base);
  await el("toggle").click();
  assert.deepEqual(plain(ctx.calls.at(-1).body), { enabled: false });
  // A server without an authority target cannot be switched.
  ctx.responses.push({ ...base, available: false });
  await el("refresh").click();
  assert.match(el("status").textContent, /^Not set up/);
  assert.equal(el("toggle").disabled, true);
  assert.match(el("help").textContent, /cannot switch automatic failover/);

  // Gating: non-admins get a plain reason and send nothing.
  const sent = ctx.calls.length;
  ctx.caps = { role: "read_only", features: {}, routes: [] };
  tools.capabilities(ctx.caps);
  assert.match(el("result").textContent, /^Automatic failover requires a super admin/);
  assert.deepEqual(disabled("refresh", "toggle"), [true, true]);
  await el("refresh").click();
  assert.equal(ctx.calls.length, sent);
  assert.equal(availability(null, false).autoFailover.allowed, false);

  // Disconnect clears the card and locks it; late results are dropped.
  tools.capabilities(caps);
  ctx.responses.push(on);
  await el("refresh").click();
  tools.disconnect();
  assert.equal(el("status").textContent, "Automatic failover status not loaded.");
  assert.deepEqual([el("result").textContent, el("facts").hidden], ["", true]);
  assert.deepEqual(disabled("refresh", "toggle"), [true, true]);
  tools.capabilities(caps);
  ctx.responses.push(() => { tools.disconnect(); return on; });
  await el("refresh").click();
  assert.equal(el("status").textContent, "Automatic failover status not loaded.");
}
