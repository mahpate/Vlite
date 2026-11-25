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
  "auto-failover-status", "auto-failover-facts", "auto-failover-refresh", "auto-failover-toggle", "auto-failover-help", "auto-failover-result",
  "tenant-deployment-status", "tenant-deployment-list", "tenant-deployment-refresh", "tenant-deployment-next", "tenant-deployment-help", "tenant-deployment-result",
  ...["operation", "operation-status", "operation-facts", "operation-help", "operation-message", "check", "cancel",
    "name", "max-vectors", "resources", "secrets", "create", "create-message",
    "readers-tenant", "readers-mode", "readers-present", "readers-min", "readers-max", "readers-routing", "readers", "readers-message",
    "retire-tenant", "retire-receipt", "retire-reason", "retire", "retire-message",
    ...["writer", "init", "reader", "scaling"].flatMap((p) => [`${p}-file`, `${p}-text`, `${p}-note`])].map((id) => `tenant-deployment-${id}`)];
const elements = Object.fromEntries(ids.map((id) => [id, element(id, id.endsWith("-result") ? "PRE" : "BUTTON")]));
// File inputs: clearing the value clears the chosen files, as in a browser.
for (const id of ids.filter((name) => name.endsWith("-file"))) {
  const e = elements[id];
  let value = "";
  e.files = [];
  Object.defineProperty(e, "value", { enumerable: true, get: () => value, set: (next) => { value = next; if (next === "") e.files = []; } });
}
const downloads = [];
// Polling timers (5 s and longer) are captured so tests advance them by hand.
const timers = [];
// This tab's sessionStorage (a saved cancel survives a reload).
const session = new Map();
const sandbox = {
  sessionStorage: { getItem: (k) => session.get(k) ?? null, setItem: (k, v) => session.set(k, String(v)), removeItem: (k) => session.delete(k),
    key: (i) => [...session.keys()][i] ?? null, get length() { return session.size; } },
  window: {}, URLSearchParams, Blob, BigInt, Number, JSON, Uint8Array, Date, crypto: globalThis.crypto, TextEncoder, TextDecoder,
  setTimeout: (fn, ms) => { if (ms >= 5000) { timers.push({ fn, ms }); return timers.length; } return setTimeout(fn, ms); },
  clearTimeout: (id) => { if (typeof id === "number") { if (timers[id - 1]) timers[id - 1].fn = null; } else clearTimeout(id); },
  // A real URL parser (the page resolves its API address with it), with object URLs captured for downloads.
  URL: Object.assign(class extends URL {}, { createObjectURL: (blob) => { downloads.push(blob); return "blob:x"; }, revokeObjectURL() {} }),
  document: {
    baseURI: "http://dash.test/console/",
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
    // app.js's apiBase (the vlite-api-base meta): requests go to `${apiBase}${path}`, here the site root.
    apiBase: "",
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
  assert.deepEqual(Object.keys(tools).sort(), ["capabilities", "disconnect", "forgetAccountCancels", "gate"]);
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
  await tenantDeploymentTests();
  await tenantDeploymentWriteTests();
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

  // Request: turning on acknowledges the warning and carries the revision it saw; turning off needs no revision.
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
  assert.match(el("help").textContent, /fresh disabled revision/);
  assert.match(el("help").textContent, /one already past that check may complete/);
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

  // Open access (no admin token): status still loads, but switching is refused before a request,
  // because the server answers 401 and that would sign the dashboard out.
  const openCaps = { role: "admin", identity: { kind: "open", label: "Open access" }, features: {}, routes: [] };
  const openGate = availability(openCaps).autoFailover;
  assert.deepEqual([openGate.allowed, openGate.change], [true, false]);
  assert.match(openGate.changeReason, /VLITE_AUTH_TOKEN/);
  ctx.caps = openCaps;
  tools.capabilities(openCaps);
  ctx.responses.push(base);
  await el("refresh").click();
  assert.equal(ctx.calls.at(-1).method, "GET");
  assert.equal(el("toggle").disabled, true);
  assert.match(el("help").textContent, /needs configured admin authentication.*Refresh status still shows the current setting/s);
  const openSent = ctx.calls.length;
  await el("toggle").click();
  assert.equal(ctx.calls.length, openSent);
  assert.match(el("result").textContent, /VLITE_AUTH_TOKEN/);
  assert.deepEqual([availability(caps).autoFailover.change, availability(caps).autoFailover.changeReason], [true, ""]);

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

// ---- Tenant deployments: read-only list of recorded operations (EPIC-011). ----
async function tenantDeploymentTests() {
  const { deploymentListPath, deploymentRequest, deploymentView, deploymentList, deploymentBlocked, deploymentExplain } = utils;
  const plain = (value) => JSON.parse(JSON.stringify(value));
  const fail = (status, code) => Object.assign(new Error(`HTTP ${status}: ${code}: x`), { status, code });
  const A = "a".repeat(32), B = "b".repeat(32), T = Date.UTC(2026, 10, 14, 12, 30);
  const op = (id, extra = {}) => ({ operation_id: id, kind: "create", tenant_id: "c".repeat(32), phase: "reserved",
    next_step: "cloud execution is blocked (OD-002); cancel to release the reservation",
    operation_generation: 1, lifecycle_generation: 1, control_generation: 3, receipt_digest: null, created_unix_ms: T,
    desired: { kind: "create", name: "acme", max_vectors: 1000, writer_profile: "d".repeat(64), initialization: "e".repeat(64), resources: [], secret_versions: 1 },
    observed: { reserved: true, routed: false, cancellation_requested: false, journal_steps: 2, uncertain_steps: 0, terminal: false }, ...extra });

  // Paths: page size 1..100 and a 32-hex cursor; anything else is refused before a request.
  assert.equal(deploymentListPath(), "/v1/admin/tenant-deployment-operations?limit=20");
  assert.equal(deploymentListPath(A), `/v1/admin/tenant-deployment-operations?limit=20&after=${A}`);
  assert.equal(deploymentListPath(null, 100), "/v1/admin/tenant-deployment-operations?limit=100");
  for (const bad of ["", "A".repeat(32), "a".repeat(31), `${A}&limit=1`, "../x"]) assert.throws(() => deploymentListPath(bad), /not a valid operation ID/, bad);
  for (const bad of [0, 101, 1.5, "20"]) assert.throws(() => deploymentListPath(null, bad), /page size/);

  // View: plain-language kind and phase; server text stays data; unknown values are labelled, not dropped.
  const view = deploymentView(op(A));
  assert.equal(view.headline, "Create tenant: Reserved, waiting for cloud execution");
  assert.deepEqual(plain(view.facts), [["Request", "Name acme; vector limit 1000"], ["Next step", "cloud execution is blocked (OD-002); cancel to release the reservation"],
    ["Requested", "2026-11-14 12:30 UTC"], ["Operation ID", A], ["Tenant ID", "c".repeat(32)], ["Recorded steps", "2"]]);
  assert.equal(deploymentView(op(A, { kind: "retire", phase: "retired" })).headline, "Retire tenant: Retired: route removed, data kept");
  assert.equal(deploymentView(op(A, { kind: "set_readers", phase: "accepted" })).headline, "Change reader replicas: Recorded, nothing reserved yet");
  assert.equal(deploymentView(op(A, { kind: "migrate", phase: "cancelling" })).headline, "Move tenant: Cancelling, not finished");
  assert.equal(deploymentView(op(A, { kind: "teleport", phase: "constructor" })).headline, "Unrecognized (teleport): Unrecognized (constructor)");
  assert.equal(deploymentView(op(A, { observed: { journal_steps: 3, uncertain_steps: 1 } })).facts[5][1], "3 (1 not confirmed)");
  assert.deepEqual(plain(deploymentView(null).facts.map(([, value]) => value)), ["--", "--", "--", "--", "--", "--"]);
  assert.equal(deploymentRequest("create", { name: "a" }), "Name a; vector limit none");
  assert.equal(deploymentRequest("set_readers", { fleet: { state: "absent" } }), "No reader replicas");
  assert.equal(deploymentRequest("set_readers", { fleet: { state: "present", min_replicas: 1, max_replicas: 3, read_routing: "readers" } }), "1 to 3 reader replicas; reads go to the readers");
  assert.equal(deploymentRequest("set_readers", { fleet: { state: "present", min_replicas: 0, max_replicas: 2, read_routing: "writer_only" } }), "0 to 2 reader replicas; reads go to the writer only");
  assert.match(deploymentRequest("retire", {}), /all data is kept/);
  assert.equal(deploymentRequest("set_readers", null), "--");
  assert.equal(deploymentRequest("set_readers", { fleet: { state: "present", min_replicas: -1, max_replicas: 2.5, read_routing: "sideways" } }),
    "? to ? reader replicas; reads go to Unrecognized (sideways)");
  assert.equal(deploymentRequest("set_readers", { fleet: { state: "frozen" } }), "Reader replicas: Unrecognized (frozen)");
  // List: shape-checked, including the cursor, which must move forward from the page it was read after.
  assert.equal(deploymentList({ operations: [op(A)], next_after: A }).next, A);
  assert.equal(deploymentList({ operations: [], next_after: null }).items.length, 0);
  assert.equal(deploymentList({ operations: [], next_after: B }, A).next, B);
  assert.equal(deploymentList({ operations: [], next_after: null }, A).next, null);
  for (const bad of [null, {}, { operations: {}, next_after: null }, { operations: [], next_after: "x" }, { operations: [] }])
    assert.throws(() => deploymentList(bad), /unexpected tenant deployment list/);
  for (const next of [A, "0".repeat(32)]) assert.throws(() => deploymentList({ operations: [], next_after: next }, A), /unexpected tenant deployment list/);

  // Refusals: "not available here" codes are explained, not treated as failures; unknown codes still show.
  assert.match(deploymentBlocked(fail(409, "tenant_deployments_not_configured")), /--tenant-deployments-file/);
  assert.match(deploymentBlocked(fail(403, "read_only")), /Read-only credentials/);
  assert.match(deploymentBlocked(fail(403, "gateway_tenant_required")), /HA gateway/);
  assert.match(deploymentBlocked(fail(404, "not_found")), /does not offer tenant deployments/);
  // Any 404 means the route is missing here, whatever code the proxy or older server used.
  assert.match(deploymentBlocked(fail(404, "route_missing")), /does not offer tenant deployments/);
  assert.match(deploymentExplain(fail(404, "route_missing")), /does not offer tenant deployments/);
  assert.equal(deploymentBlocked(fail(500, "not_found")), "");
  assert.equal(deploymentBlocked(fail(503, "operation_store_unavailable")), "");
  assert.equal(deploymentBlocked(fail(409, "constructor")), "");
  assert.equal(deploymentBlocked(new Error("network")), "");
  assert.match(deploymentExplain(fail(503, "operation_store_unavailable")), /^HTTP 503: operation_store_unavailable: x\nThe operation store could not be read/);
  assert.match(deploymentExplain(fail(400, "invalid_query")), /start again from the first page/);
  assert.match(deploymentExplain(fail(409, "constructor")), /^HTTP 409: constructor: x\nThe dashboard has no explanation for this code/);
  assert.equal(deploymentExplain(new Error("network down")), "network down");

  // Availability: super admin only; everyone else is told why, including about the HA gateway.
  assert.equal(availability({ role: "admin", features: {}, routes: [] }).tenantDeployments.allowed, true);
  const reader = availability({ role: "read_only", features: {}, routes: [] }).tenantDeployments;
  assert.equal(reader.allowed, false);
  assert.match(reader.reason, /requires a super admin.*HA gateway/s);
  assert.match(availability({ role: "scoped", scope: { tenant: "t" }, features: {}, routes: [] }).tenantDeployments.reason, /requires a super admin/);
  assert.match(availability(null, false).tenantDeployments.reason, /Sign in first/);
  // Open access (no admin token): the server answers 401, which would sign the dashboard out, so the card stays locked.
  const openTd = availability({ role: "admin", identity: { kind: "open", label: "Open access" }, features: {}, routes: [] }).tenantDeployments;
  assert.equal(openTd.allowed, false);
  assert.match(openTd.reason, /^Tenant deployments needs configured admin authentication.*VLITE_AUTH_TOKEN/s);

  // Wiring: nothing loads on sign-in; Load operations reads page 1 and Next page follows next_after.
  reset(); timers.length = 0;
  const ctx = fakeCtx();
  const caps = { role: "admin", features: {}, routes: [] };
  ctx.caps = caps;
  const tools = init(ctx);
  tools.capabilities(caps);
  const el = (id) => elements[`tenant-deployment-${id}`];
  const rows = () => el("list").children.map((item) => [item.children[0].textContent, ...item.children[1].children.map((row) => row.children[1].textContent)]);
  assert.equal(ctx.calls.length, 0);
  assert.equal(el("status").textContent, "Tenant deployment operations not loaded.");
  assert.deepEqual([el("refresh").disabled, el("next").disabled, el("list").hidden], [false, true, true]);
  assert.match(el("help").textContent, /first 20/);

  const hostile = "<img src=x onerror=alert(1)>";
  ctx.responses.push({ operations: [op(A, { next_step: hostile }), op(B, { kind: "retire", phase: "retired" })], next_after: B });
  await el("refresh").click();
  assert.deepEqual(plain(ctx.calls), [{ method: "GET", path: "/v1/admin/tenant-deployment-operations?limit=20", options: { data: false } }]);
  assert.equal(el("list").hidden, false);
  assert.equal(rows().length, 2);
  assert.equal(rows()[0][2], hostile);
  assert.equal(rows()[1][0], "Retire tenant: Retired: route removed, data kept");
  assert.equal(el("status").textContent, "Page 1: 2 operations; more on the next page. Listed in operation ID order, not by time.");
  assert.equal(el("status").attributes["data-tone"], "ready");
  assert.equal(el("next").disabled, false);
  assert.match(el("help").textContent, /after the last one listed/);

  ctx.responses.push({ operations: [op("f".repeat(32))], next_after: null });
  await el("next").click();
  assert.equal(ctx.calls[1].path, `/v1/admin/tenant-deployment-operations?limit=20&after=${B}`);
  assert.equal(el("status").textContent, "Page 2: 1 operation. Listed in operation ID order, not by time.");
  assert.equal(el("next").disabled, true);
  // Next page without a cursor sends nothing.
  await el("next").click();
  assert.equal(ctx.calls.length, 2);
  assert.match(el("result").textContent, /Select Load operations first/);

  // Load operations restarts at page 1; an empty server says so plainly.
  ctx.responses.push({ operations: [], next_after: null });
  await el("refresh").click();
  assert.equal(ctx.calls[2].path, "/v1/admin/tenant-deployment-operations?limit=20");
  assert.equal(el("status").textContent, "No tenant deployment operations are recorded yet.");
  assert.equal(el("list").hidden, true);

  // A network or server failure keeps the page and its position so the same button can try again.
  ctx.responses.push({ operations: [op(A)], next_after: A });
  await el("refresh").click();
  ctx.responses.push({ operations: [op(B)], next_after: B });
  await el("next").click();
  const kept = ctx.errors.length;
  ctx.responses.push(fail(503, "operation_store_unavailable"));
  await el("next").click();
  assert.equal(ctx.calls.at(-1).path, `/v1/admin/tenant-deployment-operations?limit=20&after=${B}`);
  assert.equal(ctx.errors.length, kept + 1);
  assert.match(el("status").textContent, /^Could not load tenant deployment operations; page 2 below is from before/);
  assert.deepEqual([rows().length, el("list").hidden, el("next").disabled], [1, false, false]);
  ctx.responses.push(new Error("network down"));
  await el("next").click();
  assert.equal(ctx.calls.at(-1).path, `/v1/admin/tenant-deployment-operations?limit=20&after=${B}`);
  assert.match(el("status").textContent, /page 2 below is from before/);
  assert.deepEqual([el("list").hidden, el("next").disabled], [false, false]);
  ctx.responses.push({ operations: [op("f".repeat(32))], next_after: null });
  await el("next").click();
  assert.equal(el("status").textContent, "Page 3: 1 operation. Listed in operation ID order, not by time.");
  // A rejected position starts over from the first page.
  ctx.responses.push({ operations: [op(A)], next_after: A });
  await el("refresh").click();
  ctx.responses.push(fail(400, "invalid_query"));
  await el("next").click();
  assert.deepEqual([el("list").hidden, el("next").disabled], [true, true]);
  assert.match(el("result").textContent, /start again from the first page/);
  // A page whose next position does not move forward is refused, so paging cannot loop.
  ctx.responses.push({ operations: [op(B)], next_after: B });
  await el("refresh").click();
  ctx.responses.push({ operations: [op(A)], next_after: A });
  await el("next").click();
  assert.match(el("result").textContent, /unexpected tenant deployment list/);
  assert.match(el("status").textContent, /^Could not load/);
  assert.deepEqual([el("list").hidden, el("next").disabled], [true, true]);
  // Any 404 reads as "not offered here", not as a failure.
  const quiet = ctx.errors.length;
  ctx.responses.push(fail(404, "route_missing"));
  await el("refresh").click();
  assert.equal(ctx.errors.length, quiet);
  assert.match(el("status").textContent, /does not offer tenant deployments/);

  // Not configured (409): a calm status, not an error; the list and cursor are cleared.
  ctx.responses.push({ operations: [op(A)], next_after: A });
  await el("refresh").click();
  const errors = ctx.errors.length;
  ctx.responses.push(fail(409, "tenant_deployments_not_configured"));
  await el("refresh").click();
  assert.equal(ctx.errors.length, errors);
  assert.match(el("status").textContent, /off on this server.*--tenant-deployments-file/);
  assert.equal(el("status").attributes["data-tone"], "blocked");
  assert.deepEqual([el("list").hidden, el("next").disabled], [true, true]);
  assert.match(el("result").textContent, /^HTTP 409: tenant_deployments_not_configured/);
  assert.match(ctx.statuses.at(-1), /--tenant-deployments-file/);
  // A reader token refused by the server (403 read_only) and the HA gateway refusal read the same way.
  ctx.responses.push(fail(403, "read_only"));
  await el("refresh").click();
  assert.match(el("status").textContent, /Read-only credentials cannot use tenant deployments/);
  ctx.responses.push(fail(403, "gateway_tenant_required"));
  await el("refresh").click();
  assert.match(el("status").textContent, /HA gateway does not serve admin routes/);
  assert.equal(ctx.errors.length, errors);
  // Other failures are reported as errors with a plain-language hint.
  ctx.responses.push(fail(503, "operation_store_unavailable"));
  await el("refresh").click();
  assert.equal(ctx.errors.length, errors + 1);
  assert.match(el("status").textContent, /Could not load/);
  assert.match(el("result").textContent, /operation store could not be read/);
  ctx.responses.push({ operations: "nope", next_after: null });
  await el("refresh").click();
  assert.match(el("result").textContent, /unexpected tenant deployment list/);

  // A read-only sign-in: both buttons are disabled with the reason, and a click sends nothing.
  const roCaps = { role: "read_only", features: {}, routes: [] };
  ctx.caps = roCaps;
  tools.capabilities(roCaps);
  assert.deepEqual([el("refresh").disabled, el("next").disabled], [true, true]);
  assert.match(el("help").textContent, /requires a super admin/);
  assert.match(el("result").textContent, /requires a super admin/);
  const sent = ctx.calls.length;
  await el("refresh").click();
  assert.equal(ctx.calls.length, sent);

  // Sign-out clears the list and locks the card; a late answer is dropped.
  ctx.caps = caps;
  tools.capabilities(caps);
  ctx.responses.push({ operations: [op(A)], next_after: A });
  await el("refresh").click();
  tools.disconnect();
  assert.equal(el("status").textContent, "Tenant deployment operations not loaded.");
  assert.deepEqual([el("list").hidden, el("refresh").disabled, el("next").disabled], [true, true, true]);
  tools.capabilities(caps);
  ctx.responses.push(() => { tools.disconnect(); return { operations: [op(A)], next_after: A }; });
  await el("refresh").click();
  assert.equal(el("status").textContent, "Tenant deployment operations not loaded.");
  assert.equal(el("list").hidden, true);
}

// ---- Tenant deployments: recording create, readers and retire requests, and cancelling an operation (ITEM-024). ----
async function tenantDeploymentWriteTests() {
  const { sha256Hex, deploymentContent, deploymentCreateSubmission, deploymentReadersSubmission, deploymentRetireSubmission, deploymentTerminal,
    deploymentCancelRequest, deploymentCancelConfirm, deploymentRetryable, deploymentProfileText, deploymentExplain, DEPLOY_HINTS, DEPLOY_SETTLED,
    deploymentAck, deploymentReleasePending, deploymentReleaseConfirm, deploymentSavedCancel, DEPLOY_PHASES, DEPLOY_KINDS, RELEASE_PENDING, RELEASE_LOST, RELEASE_REFUSED,
    deploymentPrincipal, deploymentCancelOwner, deploymentCancelScope, deploymentApiScope } = utils;
  const nodeSha = (data) => require("node:crypto").createHash("sha256").update(data).digest("hex");
  const plain = (value) => JSON.parse(JSON.stringify(value));
  const fail = (status, code) => Object.assign(new Error(`HTTP ${status}: ${code}: x`), { status, code });
  const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
  const A = "a".repeat(32), B = "b".repeat(32), T = "c".repeat(32), RC = "d".repeat(64);
  const op = (id, extra = {}) => ({ operation_id: id, kind: "create", tenant_id: T, phase: "reserved",
    next_step: "cloud execution is blocked (OD-002); cancel to release the reservation",
    operation_generation: 4, lifecycle_generation: 2, control_generation: 9, receipt_digest: null, created_unix_ms: 0,
    desired: { kind: "create", name: "acme" }, observed: { journal_steps: 2, uncertain_steps: 0, terminal: false }, ...extra });
  const file = (name, bytes) => ({ name, size: bytes.length, arrayBuffer: async () => Uint8Array.from(bytes).buffer });

  // SHA-256 in plain JS (crypto.subtle is missing on plain HTTP): FIPS 180-2 vectors, then every padding boundary against node.
  assert.equal(sha256Hex(""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
  assert.equal(sha256Hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
  assert.equal(sha256Hex("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
  assert.equal(sha256Hex("a".repeat(1000000)), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
  for (let n = 0; n <= 130; n++) {
    const bytes = Uint8Array.from({ length: n }, (_, i) => (i * 31 + n * 7) & 255);
    assert.equal(sha256Hex(bytes), nodeSha(bytes), `length ${n}`);
  }
  for (const text of ["\u00e9", "\u6f22\u5b57", "\ud83d\ude42 profile\r\n", "\ufeff{}\n"]) assert.equal(sha256Hex(text), nodeSha(Buffer.from(text, "utf8")));
  assert.throws(() => sha256Hex(42), /string or a Uint8Array/);

  // Profile text: the exact text and the SHA-256 of its UTF-8 bytes; empty, broken or over 1 MiB is refused.
  assert.deepEqual(plain(deploymentContent("{}", "Writer profile")), { text: "{}", bytes: 2, digest: nodeSha("{}") });
  assert.equal(deploymentContent("x".repeat(1048576), "W").bytes, 1048576);
  assert.throws(() => deploymentContent("x".repeat(1048577), "Writer profile"), /Writer profile is 1048577 bytes; at most 1048576/);
  assert.throws(() => deploymentContent("", "Writer profile"), /Writer profile is required: choose the file or paste its text/);
  assert.throws(() => deploymentContent("\ud800", "Writer profile"), /invalid Unicode/);
  // A chosen file is used byte for byte (BOM and CRLF kept), so the digest matches sha256sum / Get-FileHash of the file.
  assert.equal(await deploymentProfileText(null, "pasted", "Writer profile"), "pasted");
  const bom = [0xef, 0xbb, 0xbf, ...Buffer.from('{"name":"\u00e9"}\r\n')];
  const read = await deploymentProfileText(file("writer.json", bom), "ignored", "Writer profile");
  assert.equal(read, '\ufeff{"name":"\u00e9"}\r\n');
  assert.equal(deploymentContent(read, "Writer profile").digest, nodeSha(Buffer.from(bom)));
  await assert.rejects(deploymentProfileText(file("app.bin", [0xff, 0xfe, 0x00]), "", "Writer profile"), /Writer profile: app\.bin is not UTF-8 text/);
  await assert.rejects(deploymentProfileText({ name: "big.json", size: 1048577, arrayBuffer: async () => { throw new Error("read"); } }, "", "Writer profile"),
    /big\.json is 1048577 bytes; at most 1048576/);

  // Create: digests of the exact texts, each distinct text once, blank lines dropped, a blank limit omitted.
  const W = '{"apps":{"writer":"w"}}\r\n', I = "seed: empty\n";
  const create = deploymentCreateSubmission({ name: "acme", maxVectors: " 1000 ", writer: W, initialization: I,
    resources: " /subscriptions/s/a \r\n\r\n/subscriptions/s/b\n", secrets: "archive-sas@3" });
  assert.equal(create.path, "/v1/admin/tenant-deployments");
  assert.deepEqual(plain(create.request), { kind: "create", name: "acme", max_vectors: 1000, writer_profile: nodeSha(W), initialization: nodeSha(I),
    resources: ["/subscriptions/s/a", "/subscriptions/s/b"], secret_versions: ["archive-sas@3"] });
  assert.deepEqual(plain(create.contents), [W, I]);
  assert.match(create.confirm, /^Record a request to create tenant "acme" \(vector limit 1000\)\?\n/);
  assert.match(create.confirm, /Cloud execution is switched off \(OD-002\), so nothing is deployed, routed, started or stopped in Azure\./);
  assert.ok(create.confirm.includes(`Writer profile: ${Buffer.byteLength(W)} bytes, SHA-256 ${nodeSha(W)}`));
  assert.match(create.confirm, /2 Azure resources and 1 secret version listed/);
  const same = deploymentCreateSubmission({ name: "b", maxVectors: "", writer: "{}", initialization: "{}", resources: "", secrets: "" });
  assert.equal(Object.hasOwn(same.request, "max_vectors"), false);
  assert.deepEqual(plain(same.contents), ["{}"]);
  assert.deepEqual(plain([same.request.resources, same.request.secret_versions]), [[], []]);
  for (const [form, pattern] of [
    [{ name: "", writer: "{}", initialization: "{}" }, /Tenant name is required/],
    [{ name: "x".repeat(65), writer: "{}", initialization: "{}" }, /at most 64/],
    [{ name: "a", maxVectors: "0", writer: "{}", initialization: "{}" }, /whole number above 0/],
    [{ name: "a", maxVectors: "-1", writer: "{}", initialization: "{}" }, /Max vectors must be blank/],
    [{ name: "a", writer: "", initialization: "{}" }, /Writer profile is required/],
    [{ name: "a", writer: "{}", initialization: "" }, /Initialization settings is required/],
    [{ name: "a", writer: "{}", initialization: "{}", resources: "x\ty" }, /Azure resources line 1 must not contain/],
    [{ name: "a", writer: "{}", initialization: "{}", resources: "x".repeat(2049) }, /2049 bytes; at most 2048/],
    [{ name: "a", writer: "{}", initialization: "{}", secrets: Array(65).fill("s@1").join("\n") }, /Secret versions has 65 lines; at most 64/],
  ]) assert.throws(() => deploymentCreateSubmission(form), pattern);

  // Readers: a present fleet references both profiles; absent sends no contents.
  const R = "reader: {}\n", S = "scale: cpu above 70\n";
  const readers = deploymentReadersSubmission({ tenant: ` ${T.toUpperCase()} `, mode: "present", min: "1", max: " 3 ", routing: "writer_only", reader: R, scaling: S });
  assert.equal(readers.path, `/v1/admin/tenant-deployments/${T}/readers`);
  assert.deepEqual(plain(readers.request), { kind: "set_readers", tenant_id: T,
    fleet: { state: "present", min_replicas: 1, max_replicas: 3, reader_profile: nodeSha(R), scaling_rule: nodeSha(S), read_routing: "writer_only" } });
  assert.deepEqual(plain(readers.contents), [R, S]);
  assert.match(readers.confirm, /^Record a request for 1 to 3 reader replicas on tenant c{32}, with reads going to the writer only\?/);
  assert.match(readers.confirm, /OD-002[\s\S]*ITEM-008/);
  assert.equal(deploymentReadersSubmission({ tenant: T, mode: "present", min: "0", max: "1", routing: "readers", reader: R, scaling: S }).request.fleet.read_routing, "readers");
  const absent = deploymentReadersSubmission({ tenant: T, mode: "absent", min: "junk", reader: "" });
  assert.deepEqual(plain([absent.request, absent.contents]), [{ kind: "set_readers", tenant_id: T, fleet: { state: "absent" } }, []]);
  assert.match(absent.confirm, /remove all reader replicas from tenant c{32}\?\nThis only records/);
  for (const [form, pattern] of [
    [{ tenant: "x", mode: "absent" }, /Tenant ID must be the 32-character/],
    [{ tenant: T, mode: "" }, /Choose whether the tenant should have reader replicas/],
    [{ tenant: T, mode: "present", min: "1.5", max: "2", reader: R, scaling: S }, /Minimum readers must be a whole number from 0/],
    [{ tenant: T, mode: "present", min: "0", max: "0", reader: R, scaling: S }, /Maximum readers must be a whole number from 1/],
    [{ tenant: T, mode: "present", min: "3", max: "2", reader: R, scaling: S }, /must not be more than maximum/],
    [{ tenant: T, mode: "present", min: "0", max: "4294967296", reader: R, scaling: S }, /from 1 to 4294967295/],
    [{ tenant: T, mode: "present", min: "0", max: "1", routing: "sideways", reader: R, scaling: S }, /Choose where reads go/],
    [{ tenant: T, mode: "present", min: "0", max: "1", reader: "", scaling: S }, /Reader profile is required/],
    [{ tenant: T, mode: "present", min: "0", max: "1", reader: R, scaling: "" }, /Scaling rule is required/],
  ]) assert.throws(() => deploymentReadersSubmission(form), pattern);

  // Retire: the receipt from the tenant's latest finished operation; data is always retained.
  const retire = deploymentRetireSubmission({ tenant: T, receipt: ` ${RC.toUpperCase()} `, reason: "  customer left  " });
  assert.equal(retire.path, `/v1/admin/tenant-deployments/${T}/retire`);
  assert.deepEqual(plain([retire.request, retire.contents]), [{ kind: "retire", tenant_id: T, expected_receipt: RC, reason: "customer left", data_disposition: "retain" }, []]);
  assert.match(retire.confirm, /^Record a request to retire tenant c{32}\?\n.*All data is kept\.\nThis only records/);
  assert.match(retire.confirm, /Reason: customer left/);
  for (const [form, pattern] of [
    [{ tenant: T, receipt: "abc", reason: "r" }, /Receipt must be the 64-character receipt/],
    [{ tenant: T, receipt: RC, reason: " " }, /Reason is required/],
    [{ tenant: T, receipt: RC, reason: "x".repeat(513) }, /Reason is 513 bytes; at most 512/],
    [{ tenant: T, receipt: RC, reason: "a\nb" }, /Reason must not contain tabs, line breaks/],
  ]) assert.throws(() => deploymentRetireSubmission(form), pattern);

  // Cancel quotes the tokens of the status just read; ended operations cannot be cancelled.
  assert.equal(deploymentTerminal(op(A)), false);
  for (const phase of ["succeeded", "cancelled", "retired"]) assert.equal(deploymentTerminal(op(A, { phase })), true);
  assert.equal(deploymentTerminal(op(A, { phase: "cancelling", observed: { terminal: true } })), true);
  assert.equal(deploymentTerminal(null), false);
  assert.deepEqual(plain(deploymentCancelRequest(op(A), "k")), { path: `/v1/admin/tenant-deployment-operations/${A}/cancel`,
    body: { idempotency_key: "k", expected_operation_generation: 4, expected_lifecycle_generation: 2 } });
  assert.equal(deploymentCancelRequest(op(A, { receipt_digest: RC }), "k").body.expected_receipt, RC);
  for (const bad of [null, op("x"), op(A, { operation_generation: "4" }), op(A, { lifecycle_generation: undefined })])
    assert.throws(() => deploymentCancelRequest(bad, "k"), /unexpected operation status/);
  const confirmText = deploymentCancelConfirm(op(A), false);
  assert.match(confirmText, /^Cancel operation a{32}\?\nIt is: Create tenant for tenant c{32}, now Reserved, waiting for cloud execution\.\n/);
  assert.match(confirmText, /releases it\. For a create, the tenant record stays\.\nThis only records the cancellation on this server\. Cloud execution is switched off \(OD-002\), so nothing is deployed/);
  assert.doesNotMatch(confirmText, /same request key/);
  assert.match(deploymentCancelConfirm(op(A, { kind: "set_readers", phase: "accepted" }), true), /Change reader replicas[\s\S]*withdraws it for good[\s\S]*same request key/);

  // Retry rules: an unknown outcome keeps the key; a definite answer discards it.
  for (const error of [new Error("network down"), fail(408, "timeout"), fail(429, "rate_limited"), fail(500, "internal_error"), fail(503, "operation_store_unavailable"),
    fail(409, "lifecycle_conflict"), fail(422, "invalid_input")]) assert.equal(deploymentRetryable(error), true, error.message);
  for (const error of [fail(400, "operation_request_invalid"), fail(409, "operation_request_conflict"), fail(409, "operation_generation_stale"),
    fail(404, "operation_not_found"), fail(404, "route_missing"), fail(401, "admin_auth_required"), fail(403, "read_only"), fail(409, "transition_unavailable")])
    assert.equal(deploymentRetryable(error), false, error.message);
  assert.match(deploymentExplain(Object.assign(fail(503, "operation_store_unavailable"), { deployRetry: true })),
    /^HTTP 503: operation_store_unavailable: x\nThe operation store could not be read or written right now\. Try again shortly\.\nThe server may already have recorded this/);
  // invalid_input may follow a recorded request, so the key stays; but an immediate resend fails the same way, so the
  // message says to fix the cause first (deleted tenants still count towards the 1,024 limit) and not to edit the form.
  const invalidInput = deploymentExplain(Object.assign(fail(422, "invalid_input"), { deployRetry: true }));
  assert.match(invalidInput, /^HTTP 422: invalid_input: x\nThe server may already have recorded/);
  assert.match(invalidInput, /Resending does not help until the cause is fixed[\s\S]*deleted tenants still count[\s\S]*server operator[\s\S]*without changing the form[\s\S]*recorded at most once[\s\S]*stays listed/);
  assert.doesNotMatch(invalidInput, /without changing anything/);
  assert.doesNotMatch(deploymentExplain(fail(422, "invalid_input")), /Resending does not help/);
  assert.match(deploymentExplain(fail(409, "operation_generation_stale")), /Select Check status to read it again/);
  assert.match(deploymentExplain(fail(404, "operation_not_found")), /No operation with this ID is recorded/);

  // Every refusal code the tenant deployment routes can raise has a plain explanation (kept in sync with the server).
  const sourceOf = (path) => { const text = fs.readFileSync(path, "utf8"); const cut = text.search(/#\[cfg\(test\)\]\s*mod tests/); return cut < 0 ? text : text.slice(0, cut); };
  const raising = /(?:ApiError::new\(\s*StatusCode::[A-Z_]+,\s*|\bconflict\(\s*|lifecycle_error\(\s*|\bunavailable\(\s*)"([a-z][a-z0-9_]*)"/g;
  const raised = new Set();
  for (const path of ["src/server/tenant_provisioning.rs", "src/server/tenant_provisioning/admin.rs", "src/server/control_state.rs"])
    for (const match of sourceOf(path).matchAll(raising)) raised.add(match[1]);
  // Raised only by the cloud worker, actuators, credential delivery, migration and rollback, or control-state setup: never by the routes this panel calls.
  const NOT_FROM_PANEL_ROUTES = new Set(["activation_route_invalid", "actuator_fence_unproven", "actuator_not_owner", "authority_identity_mismatch",
    "candidate_revocation_unconfirmed", "control_state_exists", "credential_delivery_unacknowledged", "credential_view_unpublished",
    "delivery_candidate_exists", "delivery_conflict", "delivery_stale", "directory_invalid", "migration_source_released",
    "projection_freshness_unproven", "readiness_unproven", "rollback_requires_reconciliation", "seed_mismatch", "seed_unrecorded",
    "tenant_migration_ack_required"]);
  assert.ok(raised.size > 40, "the error code scan found too few codes; check the pattern");
  assert.deepEqual([...raised].filter((code) => !Object.hasOwn(DEPLOY_HINTS, code) && !NOT_FROM_PANEL_ROUTES.has(code)).sort(), [],
    "new tenant deployment error codes need a plain explanation in DEPLOY_HINTS (admin-tools.js)");
  assert.deepEqual([...NOT_FROM_PANEL_ROUTES].filter((code) => !raised.has(code)), [], "codes no longer raised: drop them from NOT_FROM_PANEL_ROUTES");
  const rustFiles = (dir) => fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => entry.isDirectory() ? rustFiles(`${dir}/${entry.name}`)
    : entry.name.endsWith(".rs") ? [`${dir}/${entry.name}`] : []);
  const server = rustFiles("src/server").map((path) => fs.readFileSync(path, "utf8")).join("\n");
  assert.deepEqual(Object.keys(DEPLOY_HINTS).filter((code) => !server.includes(`"${code}"`)), [], "explained codes the server no longer uses");
  for (const code of ["tenant_deployments_not_configured", "admin_auth_required", "read_only", "forbidden_scope", "gateway_tenant_required", "not_found",
    "operation_not_found", "tenant_deployment_action_unknown", "operation_request_invalid", "operation_request_conflict", "action_request_conflict",
    "operation_generation_stale", "pin_scope_unproven", "retirement_required", "operation_terminal", "readers_in_progress", "retirement_in_progress",
    "withdrawal_capacity", "transition_unavailable", "irreversible_ack_required", "tenants_disabled", "admin_dir_busy"]) assert.ok(Object.hasOwn(DEPLOY_HINTS, code), code);
  for (const code of DEPLOY_SETTLED) assert.ok(Object.hasOwn(DEPLOY_HINTS, code), code);
  for (const [code, hint] of Object.entries(DEPLOY_HINTS)) assert.ok(typeof hint === "string" && hint.length > 20 && !/[<>]/.test(hint), code);
  // Codes that can follow a durable record say so; invalid_input is not settled, so it does not claim a definite outcome.
  assert.doesNotMatch(DEPLOY_HINTS.invalid_input, /^The request is recorded/);
  assert.match(DEPLOY_HINTS.invalid_input, /may already have recorded[\s\S]*fails the same way/);
  assert.match(DEPLOY_HINTS.tenant_id_conflict, /stays recorded \(Load operations lists it\)/);
  assert.match(DEPLOY_HINTS.tenants_commit_indeterminate, /refresh the Tenants list[\s\S]*same request key/);
  for (const code of ["readers_in_progress", "retirement_in_progress", "withdrawal_capacity"]) assert.match(DEPLOY_HINTS[code], /cancel may stay recorded/, code);

  // Whitespace-only profiles are refused before anything is sent.
  assert.throws(() => deploymentContent("  \r\n\t\n", "Writer profile"), /Writer profile contains only spaces or blank lines/);

  // The same bytes hash the same here and in the server (tenant_provisioning::sha256_hex; see its tests.rs twin).
  assert.equal(sha256Hex("\ufeffline1\r\nna\u00efve \u2013 \u65e5\u672c \ud83d\ude00\r\n"), "e07ad4b95327338923a222fd2dabade046c608d400549667038c9a51f046d3fa");

  // A 2xx answer only counts when it names the operation (and kind/tenant) that was asked for.
  assert.equal(deploymentAck({ operation: op(A) }, { operation: A, kind: "create", tenant: T }).operation_id, A);
  for (const [value, expect] of [[null, {}], [{}, {}], [{ operation: "x" }, {}], [{ operation: op("x") }, {}], [{ operation: op(A) }, { operation: B }],
    [{ operation: op(A) }, { kind: "retire" }], [{ operation: op(A) }, { tenant: B }]]) assert.equal(deploymentAck(value, expect), null, JSON.stringify(expect));

  // A cancelled operation that still holds its reservation is released by resending the earlier cancel or by a new one.
  assert.equal(deploymentReleasePending(op(A, { phase: "cancelled", observed: { terminal: true, reserved: true } })), true);
  for (const other of [op(A, { phase: "cancelled", observed: { terminal: true, reserved: false } }), op(A, { observed: { reserved: true } }), null])
    assert.equal(deploymentReleasePending(other), false);
  assert.match(deploymentReleaseConfirm(op(A, { phase: "cancelled" }), true), /^Finish cancelling operation a{32}\?\n[\s\S]*resends your earlier cancel exactly, with the same request key/);
  const freshRelease = deploymentReleaseConfirm(op(A, { phase: "cancelled" }), false);
  assert.match(freshRelease, /^Finish cancelling operation a{32}\?\n[\s\S]*sends a new cancel that quotes the status just read[\s\S]*the server refuses it and nothing changes[\s\S]*nothing in Azure is touched/);
  assert.doesNotMatch(freshRelease, /same request key/);
  assert.match(RELEASE_LOST, /^This operation is cancelled, but the server has not released[\s\S]*another account sent it from this tab[\s\S]*can sign in here again to resend it[\s\S]*select Cancel to send a new cancel[\s\S]*should then finish releasing the reservation \(a server older than this dashboard refuses it, and nothing changes\)\.$/);
  assert.doesNotMatch(RELEASE_LOST, /refused|report the operation ID/);
  // The refusal is the same when another administrator just finished the release and when the server is older.
  assert.match(RELEASE_REFUSED, /^The server refused a new cancel[\s\S]*may have just finished the release: select Check status[\s\S]*still held[\s\S]*can sign in here again to resend it; otherwise report the operation ID to the server operator\. Cancel stays off for this operation[\s\S]*\.$/);
  // The tab keeps a saved cancel only for the verified account that sent it, until it signs out or its session ends.
  assert.match(RELEASE_LOST, /only for the account that sent it[\s\S]*signs out or its session ends/);
  // Whose a saved cancel is: a verified account (the server's actor names), never a role alone; anything else is no account.
  const ALICE = "a1".repeat(16), BOB = "b2".repeat(16), STORE = "vlite.tenant-deployment.cancel.";
  const userCaps = (id, accountRole = "super_admin") => ({ role: "admin", identity: { kind: "user", user_id: id, account_role: accountRole, label: "x" }, features: {}, routes: [] });
  const tokenCaps = { role: "admin", identity: { kind: "super_admin", label: "Super admin (bootstrap admin token)" }, features: {}, routes: [] };
  assert.equal(deploymentPrincipal(userCaps(ALICE)), `user:${ALICE}`);
  assert.equal(deploymentPrincipal(tokenCaps), "admin_token");
  for (const caps of [null, {}, { role: "admin" }, { role: "admin", features: {}, routes: [] }, { role: "admin", identity: "user" },
    { role: "admin", identity: { kind: "open", label: "Open access" } }, { role: "read_only", identity: tokenCaps.identity },
    { role: "read_only", identity: userCaps(ALICE).identity }, { role: "scoped", identity: userCaps(ALICE, "editor").identity },
    userCaps(ALICE, "editor"), userCaps(ALICE, "viewer"), userCaps(ALICE.toUpperCase()), userCaps("a1"), userCaps(undefined), userCaps(null),
    { role: "admin", identity: { kind: "read_only" } }, { role: "admin", identity: { kind: "admin_token" } }])
    assert.equal(deploymentPrincipal(caps), null, JSON.stringify(caps));
  // Storage keys are filed under the API address the page sends requests to, then the owner. sessionStorage is shared by
  // every page of a site in this tab, and a dashboard can reach its API under a path (the Docker image serves it at /api).
  const PAGE = "http://dash.test/console/", ROOT = "http://dash.test", PROXIED = "http://dash.test/vlite-b/api";
  assert.equal(deploymentApiScope("", PAGE), ROOT);
  assert.equal(deploymentApiScope("", "http://dash.test/console/index.html?tab=admin#tenants"), ROOT);
  assert.equal(deploymentApiScope("/vlite-b/api", PAGE), PROXIED);
  // Exactly the prefix the requests use (fetch resolves `${apiBase}/v1/...` against the page), never a guess about it.
  assert.equal(deploymentApiScope("/vlite-b/api/", PAGE), `${PROXIED}/`);
  assert.equal(deploymentApiScope("api", PAGE), "http://dash.test/console/api");
  assert.equal(deploymentApiScope("https://api.test:8443/v", PAGE), "https://api.test:8443/v");
  // No address can be established, so nothing is saved: no base or page, a page that is not http(s), or a base that
  // carries credentials, a query or a fragment (these are never put in a key).
  for (const [base, page] of [[undefined, PAGE], [null, PAGE], [1, PAGE], ["", undefined], ["", "about:blank"], ["", "file:///c/dash.html"],
    ["https://user:pw@api.test/v", PAGE], ["https://token@api.test/v", PAGE], ["/api?key=secret", PAGE], ["/api#x", PAGE], ["javascript:void 0;//", PAGE]])
    assert.equal(deploymentApiScope(base, page), null, `${base} @ ${page}`);
  const keyIn = (scope, who, id) => `${STORE}${encodeURIComponent(scope)}/${who}.${id}`;
  assert.equal(keyIn(ROOT, "admin_token", A), `${STORE}http%3A%2F%2Fdash.test/admin_token.${A}`);
  assert.equal(deploymentCancelOwner(keyIn(ROOT, `user:${ALICE}`, A), ROOT), `user:${ALICE}`);
  assert.equal(deploymentCancelOwner(keyIn(ROOT, "admin_token", A), ROOT), "admin_token");
  assert.equal(deploymentCancelOwner(keyIn(PROXIED, "admin_token", A), PROXIED), "admin_token");
  // Another API address's keys are not this page's (undefined): never read and never deleted, whatever they hold.
  for (const key of [keyIn(PROXIED, `user:${ALICE}`, A), keyIn(PROXIED, "admin_token", A), keyIn(PROXIED, "user:nope", A), `${STORE}${encodeURIComponent(PROXIED)}/`,
    keyIn(`${ROOT}/`, "admin_token", A)])
    assert.equal(deploymentCancelOwner(key, ROOT), undefined, key);
  // Unknown (null, deleted, never used): an older page's key filed under no address (the operation alone, or owner and
  // operation), or a damaged key under this address.
  for (const unknown of [`${STORE}${A}`, STORE, `${STORE}user:${ALICE}.${A}`, `${STORE}admin_token.${A}`, `${STORE}/admin_token.${A}`,
    keyIn(ROOT, `user:${ALICE.toUpperCase()}`, A), keyIn(ROOT, `user:${ALICE}`, "nope"), `${STORE}${encodeURIComponent(ROOT)}/user:${ALICE}`,
    `${STORE}${encodeURIComponent(ROOT)}/`, keyIn(ROOT, "open", A), keyIn(ROOT, "read_only_token", A), `${keyIn(ROOT, `user:${ALICE}`, A)}.x`])
    assert.equal(deploymentCancelOwner(unknown, ROOT), null, unknown);
  // A page that cannot establish its address owns no key, but still recognizes (and deletes) an older page's.
  assert.equal(deploymentCancelOwner(keyIn(ROOT, "admin_token", A), null), undefined);
  assert.equal(deploymentCancelOwner(`${STORE}admin_token.${A}`, null), null);
  for (const other of ["vlite.theme", "", `x${STORE}admin_token.${A}`]) assert.equal(deploymentCancelOwner(other, ROOT), undefined, other);
  // What the page says about a saved cancel's reach, by owner.
  const aliceScope = deploymentCancelScope(`user:${ALICE}`, ROOT);
  assert.match(aliceScope, /never a password or token[\s\S]*for the account that sent it[\s\S]*same account signed in again can resend it, as long as its session did not end[\s\S]*Signing out, or the session ending, deletes it[\s\S]*Another account signed in to this tab cannot resend it[\s\S]*plain text that anyone using this tab can read/);
  assert.match(aliceScope, /under the API address this dashboard uses[\s\S]*a dashboard on this site that uses another API address neither resends nor deletes it/);
  assert.doesNotMatch(aliceScope, /cannot see/);
  assert.doesNotMatch(aliceScope, /bootstrap admin token/);
  assert.match(deploymentCancelScope("admin_token", ROOT), /Everyone signed in with the bootstrap admin token counts as one account[\s\S]*rejected token counts as the session ending/);
  assert.match(deploymentCancelScope(null, ROOT), /did not identify your account, so that cancel is not saved[\s\S]*a reload, a sign-out or another sign-in loses it/);
  for (const who of [`user:${ALICE}`, "admin_token"])
    assert.match(deploymentCancelScope(who, null), /could not tell which API address it uses, so that cancel is not saved[\s\S]*a reload, a sign-out or another sign-in loses it/);
  assert.match(RELEASE_LOST, /for the account signed in now under this dashboard's API address/);
  const savedBody = { idempotency_key: "k", expected_operation_generation: 4, expected_lifecycle_generation: 2 };
  const saved = { operation: A, principal: `user:${ALICE}`, scope: ROOT, path: `/v1/admin/tenant-deployment-operations/${A}/cancel`, body: savedBody };
  assert.deepEqual(plain(deploymentSavedCancel(saved, A, `user:${ALICE}`, ROOT)), plain(saved));
  assert.equal(deploymentSavedCancel({ ...saved, body: { ...savedBody, expected_receipt: RC } }, A, `user:${ALICE}`, ROOT).body.expected_receipt, RC);
  for (const bad of [null, { ...saved, operation: B }, { ...saved, path: "/v1/admin/tenants" }, { ...saved, body: { ...savedBody, extra: 1 } },
    { ...saved, body: { ...savedBody, idempotency_key: "" } }, { ...saved, body: { ...savedBody, expected_operation_generation: "4" } },
    { ...saved, principal: `user:${BOB}` }, { ...saved, principal: undefined }, { ...saved, principal: "admin_token" },
    { ...saved, scope: PROXIED }, { ...saved, scope: `${ROOT}/` }, { ...saved, scope: undefined }])
    assert.equal(deploymentSavedCancel(bad, A, `user:${ALICE}`, ROOT), null);
  // Without a verified account or an API address there is nothing to match, so nothing is used (fails closed).
  for (const who of [null, undefined, ""]) {
    assert.equal(deploymentSavedCancel(saved, A, who, ROOT), null);
    assert.equal(deploymentSavedCancel({ ...saved, principal: who }, A, who, ROOT), null);
  }
  for (const where of [null, undefined, ""]) {
    assert.equal(deploymentSavedCancel(saved, A, `user:${ALICE}`, where), null);
    assert.equal(deploymentSavedCancel({ ...saved, scope: where }, A, `user:${ALICE}`, where), null);
  }

  // The phase and kind tables match the server's enums, so a new server phase or kind fails here.
  const camel = (name) => name.replace(/[A-Z]/g, (c, i) => (i ? "_" : "") + c.toLowerCase());
  const variants = (source, name) => {
    const body = source.match(new RegExp(`pub(?:\\([a-z]+\\))? enum ${name}\\s*\\{([\\s\\S]*?)\\n\\}`))?.[1];
    assert.ok(body, `enum ${name} not found`);
    return [...body.replace(/\/\/.*$/gm, "").replace(/#\[[^\]]*\]/g, "").matchAll(/^\s*([A-Z][A-Za-z]*)\s*(?:[,{(]|$)/gm)].map((m) => camel(m[1]));
  };
  assert.deepEqual(Object.keys(DEPLOY_KINDS).sort(), variants(sourceOf("src/server/control_state.rs"), "OperationKind").sort());
  const terminal = variants(sourceOf("src/server/tenant_provisioning.rs"), "TerminalOutcome").sort();
  assert.deepEqual(terminal, ["cancelled", "retired", "succeeded"]);
  for (const phase of terminal) assert.equal(deploymentTerminal({ phase, observed: {} }), true, phase);
  const admin = sourceOf("src/server/tenant_provisioning/admin.rs");
  const render = admin.match(/let \(phase, next_step\) = match [\s\S]*?\n    \};/)?.[0];
  assert.ok(render, "the phase match in admin.rs render() was not found");
  const rendered = [...new Set([...render.matchAll(/=>\s*\(\s*"([a-z_]+)"/g)].map((m) => m[1]))].sort();
  assert.deepEqual(Object.keys(DEPLOY_PHASES).sort(), rendered, "DEPLOY_PHASES must list exactly the phases admin.rs renders");
  assert.deepEqual(Object.keys(DEPLOY_PHASES).filter((phase) => deploymentTerminal({ phase, observed: {} })).sort(), terminal);

  // ---- Wiring against a fake backend ----
  reset(); timers.length = 0;
  const ctx = fakeCtx();
  const caps = userCaps(ALICE);
  ctx.caps = caps;
  const tools = init(ctx);
  tools.capabilities(caps);
  const el = (id) => elements[`tenant-deployment-${id}`];
  const FIELDS = ["operation", "name", "max-vectors", "resources", "secrets", "readers-tenant", "readers-mode", "readers-min", "readers-max", "readers-routing",
    "retire-tenant", "retire-receipt", "retire-reason", ...["writer", "init", "reader", "scaling"].flatMap((p) => [`${p}-file`, `${p}-text`])];
  const BUTTONS = ["check", "cancel", "create", "readers", "retire"];
  const disabled = () => [...FIELDS, ...BUTTONS].filter((id) => el(id).disabled);
  assert.deepEqual(disabled(), []);
  assert.equal(ctx.calls.length, 0);
  assert.equal(el("operation-status").textContent, "No operation selected.");
  assert.match(el("operation-help").textContent, /^Check status reads/);
  const listed = (ops = []) => ({ operations: ops, next_after: null });

  // Create: a declined confirmation sends nothing.
  el("name").value = "acme"; el("max-vectors").value = "1000";
  el("writer-file").files = [file("writer.json", [...Buffer.from(W)])];
  el("writer-file").change();
  assert.match(el("writer-note").textContent, /^Using the file writer\.json \(25 bytes\) exactly as it is/);
  el("init-text").value = I;
  el("resources").value = "/subscriptions/s/a"; el("secrets").value = "archive-sas@3";
  ctx.answer = false;
  await el("create").click();
  assert.equal(ctx.calls.length, 0);
  assert.match(ctx.confirms.at(-1), /^Record a request to create tenant "acme"/);
  assert.ok(ctx.confirms.at(-1).includes(nodeSha(W)));
  assert.equal(el("create-message").textContent, "Nothing was sent.");
  // An unknown outcome (503 or no answer) keeps the request key: the same button repeats the same request.
  ctx.answer = true;
  ctx.responses.push(fail(503, "operation_store_unavailable"));
  await el("create").click();
  const first = ctx.calls.at(-1);
  assert.deepEqual([first.method, first.path, plain(first.options)], ["POST", "/v1/admin/tenant-deployments", {
    data: false, inspectorSummary: { documents: 2, resources: 1, secret_versions: 1, profile_digests: [nodeSha(W), nodeSha(I)] },
  }]);
  assert.deepEqual(Object.keys(first.body), ["idempotency_key", "request", "contents"]);
  assert.match(first.body.idempotency_key, uuid);
  assert.deepEqual(plain(first.body.request), { kind: "create", name: "acme", max_vectors: 1000, writer_profile: nodeSha(W), initialization: nodeSha(I),
    resources: ["/subscriptions/s/a"], secret_versions: ["archive-sas@3"] });
  assert.deepEqual(plain(first.body.contents), [W, I]);
  assert.match(el("create-message").textContent, /operation store could not be read[\s\S]*Select the same button again without changing anything/);
  ctx.responses.push(new Error("network down"));
  await el("create").click();
  assert.equal(ctx.calls.at(-1).body.idempotency_key, first.body.idempotency_key);
  assert.match(ctx.confirms.at(-1), /repeats your earlier request whose outcome is unknown, with the same request key/);
  // Editing any field of the form makes the next submission a new request with a new key.
  el("secrets").value = "archive-sas@4"; el("secrets").input();
  ctx.responses.push(fail(400, "operation_request_invalid"));
  await el("create").click();
  const second = ctx.calls.at(-1).body.idempotency_key;
  assert.notEqual(second, first.body.idempotency_key);
  assert.doesNotMatch(ctx.confirms.at(-1), /repeats your earlier request/);
  assert.match(el("create-message").textContent, /^HTTP 400: operation_request_invalid: x\nThe server refused a field/);
  assert.doesNotMatch(el("create-message").textContent, /same button again/);
  // A definite refusal discards the key, even when nothing was edited.
  ctx.responses.push(fail(503, "operation_store_unavailable"));
  await el("create").click();
  const third = ctx.calls.at(-1).body.idempotency_key;
  assert.notEqual(third, second);
  // invalid_input (for example "tenant capacity reached") may follow a recorded request: the key stays for the same
  // form, and the message says to fix the cause before resending instead of resending at once.
  ctx.responses.push(fail(422, "invalid_input"));
  await el("create").click();
  assert.equal(ctx.calls.at(-1).body.idempotency_key, third);
  assert.match(el("create-message").textContent, /^HTTP 422: invalid_input: x\nThe server may already have recorded[\s\S]*Resending does not help until the cause is fixed[\s\S]*without changing the form/);
  assert.doesNotMatch(el("create-message").textContent, /without changing anything/);
  // A 2xx answer that does not name a matching operation is an unknown outcome: the form and the key stay for an exact resend.
  ctx.responses.push({ duplicate: false, operation: op(A, { kind: "retire" }) });
  await el("create").click();
  assert.equal(ctx.calls.at(-1).body.idempotency_key, third);
  assert.match(el("create-message").textContent, /did not confirm this request[\s\S]*same button again/);
  assert.equal(el("name").value, "acme");
  // Success (here a duplicate): said plainly; the form clears; the operation box and the list show the result.
  const hostile = "<img src=x onerror=alert(1)>";
  const recorded = op(A, { next_step: hostile });
  ctx.responses.push({ duplicate: true, operation: recorded }, listed([recorded]));
  await el("create").click();
  assert.equal(ctx.calls.at(-2).body.idempotency_key, third);
  assert.equal(ctx.calls.at(-1).path, "/v1/admin/tenant-deployment-operations?limit=20");
  assert.equal(el("create-message").textContent, [
    "This was already recorded earlier with the same request key; nothing new was added.",
    "Create tenant: Reserved, waiting for cloud execution",
    `Next step: ${hostile}`,
    `Operation ID: ${A} (now in the operation box below, where Cancel withdraws it)`,
    `Tenant ID: ${T}`,
  ].join("\n"));
  assert.deepEqual([el("name").value, el("max-vectors").value, el("writer-file").files.length, el("init-text").value, el("resources").value, el("secrets").value,
    el("writer-note").textContent], ["", "", 0, "", "", "", ""]);
  assert.equal(el("operation").value, A);
  assert.equal(el("operation-status").textContent, "Create tenant: Reserved, waiting for cloud execution");
  assert.equal(el("operation-facts").hidden, false);
  assert.equal(el("list").children.length, 1);
  // The next create is a new request, and a fresh answer says it was recorded.
  el("name").value = "beta"; el("writer-text").value = "{}"; el("init-text").value = "{}";
  ctx.responses.push({ duplicate: false, operation: op(B, { phase: "accepted" }) }, listed());
  await el("create").click();
  assert.notEqual(ctx.calls.at(-2).body.idempotency_key, third);
  assert.deepEqual(plain(ctx.calls.at(-2).body.contents), ["{}"]);
  assert.match(el("create-message").textContent, /^Recorded\. Nothing was deployed \(cloud execution is switched off, OD-002\)\.\nCreate tenant: Recorded, nothing reserved yet\n/);
  // Validation errors are shown in the form and send nothing.
  const before = ctx.calls.length;
  el("name").value = "acme";
  await el("create").click();
  assert.match(el("create-message").textContent, /Writer profile is required: choose the file or paste its text; no request sent/);
  el("writer-file").files = [file("bin.dat", [0xff])];
  el("init-text").value = "{}";
  await el("create").click();
  assert.match(el("create-message").textContent, /bin\.dat is not UTF-8 text/);
  assert.equal(ctx.calls.length, before);
  // Pasting replaces a chosen file (and says so).
  el("writer-text").value = "{}"; el("writer-text").input();
  assert.deepEqual([el("writer-file").files.length, el("writer-note").textContent], [0, "Using the pasted text instead of the file."]);

  // Readers: profiles hidden and ignored when the tenant should have no readers.
  el("readers-tenant").value = T; el("readers-min").value = "1"; el("readers-max").value = "2";
  el("reader-text").value = R; el("scaling-file").files = [file("scale.json", [...Buffer.from(S)])];
  el("readers-mode").value = "present"; el("readers-routing").value = "readers";
  ctx.responses.push({ duplicate: false, operation: op(B, { kind: "set_readers", phase: "accepted" }) }, listed());
  await el("readers").click();
  assert.equal(ctx.calls.at(-2).path, `/v1/admin/tenant-deployments/${T}/readers`);
  assert.deepEqual(plain(ctx.calls.at(-2).body.request.fleet), { state: "present", min_replicas: 1, max_replicas: 2, reader_profile: nodeSha(R), scaling_rule: nodeSha(S), read_routing: "readers" });
  assert.deepEqual(plain(ctx.calls.at(-2).body.contents), [R, S]);
  assert.deepEqual(plain(ctx.calls.at(-2).options.inspectorSummary), {
    documents: 2, resources: 0, secret_versions: 0, profile_digests: [nodeSha(R), nodeSha(S)],
  });
  assert.match(ctx.confirms.at(-1), /ITEM-008/);
  assert.match(el("readers-message").textContent, /^Recorded\. Nothing was deployed \(cloud execution is switched off, OD-002\)\.\nChange reader replicas: Recorded, nothing reserved yet/);
  assert.deepEqual([el("readers-tenant").value, el("readers-min").value, el("readers-mode").value, el("readers-routing").value, el("scaling-file").files.length],
    ["", "", "present", "writer_only", 0]);
  el("readers-mode").value = "absent"; el("readers-mode").change();
  assert.equal(el("readers-present").hidden, true);
  el("readers-tenant").value = T;
  el("reader-file").files = [{ name: "never.json", size: 2, arrayBuffer: async () => { throw new Error("must not be read"); } }];
  ctx.responses.push({ duplicate: false, operation: op(B, { kind: "set_readers", phase: "accepted" }) }, listed());
  await el("readers").click();
  assert.deepEqual(plain([ctx.calls.at(-2).body.request, ctx.calls.at(-2).body.contents]), [{ kind: "set_readers", tenant_id: T, fleet: { state: "absent" } }, []]);
  assert.equal(el("readers-present").hidden, false);

  // "Use this operation" fills the operation, tenant and receipt in; Retire then quotes that receipt.
  const finished = op(A, { phase: "succeeded", receipt_digest: RC, observed: { journal_steps: 5, uncertain_steps: 0, terminal: true } });
  ctx.responses.push(listed([finished]));
  await el("refresh").click();
  const item = el("list").children[0];
  assert.deepEqual(item.children[1].children.at(-1).children.map((cell) => cell.textContent), ["Receipt (needed to retire this tenant)", RC]);
  const use = item.children[2];
  assert.equal(use.textContent, "Use this operation");
  // Elements made by the page fake record downloads on click(), so run the button's listener directly.
  assert.equal(use.listeners.length, 1);
  await use.listeners[0]();
  assert.deepEqual([el("operation").value, el("readers-tenant").value, el("retire-tenant").value, el("retire-receipt").value], [A, T, T, RC]);
  assert.match(el("operation-message").textContent, /^Filled in operation a{32} and its tenant ID c{32} in the Readers and Retire forms, with its receipt\./);
  el("retire-reason").value = "customer left";
  ctx.responses.push({ duplicate: false, operation: op(B, { kind: "retire", phase: "accepted" }) }, listed());
  await el("retire").click();
  assert.equal(ctx.calls.at(-2).path, `/v1/admin/tenant-deployments/${T}/retire`);
  assert.deepEqual(plain(ctx.calls.at(-2).body), { idempotency_key: ctx.calls.at(-2).body.idempotency_key, contents: [],
    request: { kind: "retire", tenant_id: T, expected_receipt: RC, reason: "customer left", data_disposition: "retain" } });
  assert.match(ctx.confirms.at(-1), /retire tenant c{32}\?[\s\S]*All data is kept/);
  assert.deepEqual([el("retire-tenant").value, el("retire-receipt").value, el("retire-reason").value], ["", "", ""]);
  // A refusal of the retirement is explained in its form.
  el("retire-tenant").value = T; el("retire-receipt").value = RC; el("retire-reason").value = "again";
  ctx.responses.push(fail(409, "retirement_in_progress"));
  await el("retire").click();
  assert.match(el("retire-message").textContent, /retirement of this tenant already started/);
  // Using an operation without a receipt empties the receipt box, so another tenant is never paired with this receipt.
  const otherTenant = "e".repeat(32);
  ctx.responses.push(listed([op(B, { tenant_id: otherTenant })]));
  await el("refresh").click();
  await el("list").children[0].children[2].listeners[0]();
  assert.deepEqual([el("retire-tenant").value, el("retire-receipt").value], [otherTenant, ""]);
  assert.match(el("operation-message").textContent, /it has no receipt, so the Retire receipt box was emptied/);

  // Cancel reads the current status first, names the operation in the confirmation, then quotes the fresh tokens.
  el("operation").value = ` ${A.toUpperCase()} `; el("operation").input();
  assert.equal(el("cancel").disabled, false);
  ctx.answer = false;
  ctx.responses.push({ operation: op(A, { operation_generation: 7, lifecycle_generation: 3 }) });
  const sentBefore = ctx.calls.length;
  await el("cancel").click();
  assert.deepEqual(plain(ctx.calls.slice(sentBefore)), [{ method: "GET", path: `/v1/admin/tenant-deployment-operations/${A}`, options: { data: false } }]);
  assert.match(ctx.confirms.at(-1), /^Cancel operation a{32}\?\nIt is: Create tenant for tenant c{32}, now Reserved/);
  assert.equal(el("operation-message").textContent, "Nothing was sent.");
  ctx.answer = true;
  const cancelled = op(A, { phase: "cancelled", operation_generation: 8, observed: { journal_steps: 3, uncertain_steps: 0, terminal: true } });
  ctx.responses.push({ operation: op(A, { operation_generation: 7, lifecycle_generation: 3 }) }, { action: "cancel", operation: cancelled }, listed([cancelled]));
  await el("cancel").click();
  const cancel = ctx.calls.at(-2);
  assert.deepEqual([ctx.calls.at(-3).method, cancel.method, cancel.path], ["GET", "POST", `/v1/admin/tenant-deployment-operations/${A}/cancel`]);
  assert.match(cancel.body.idempotency_key, uuid);
  assert.deepEqual(plain(cancel.body), { idempotency_key: cancel.body.idempotency_key, expected_operation_generation: 7, expected_lifecycle_generation: 3 });
  assert.match(el("operation-message").textContent, /^Cancelled\. Cloud execution is switched off \(OD-002\), so nothing in Azure was touched\.\nCreate tenant: Cancelled\n/);
  // An ended operation cannot be cancelled: the button is disabled while its ID is in the box.
  assert.equal(el("cancel").disabled, true);
  assert.match(el("operation-help").textContent, /has ended, so it cannot be cancelled/);
  // Check status of an ended operation says so; a click on Cancel that still arrives sends no cancel.
  ctx.responses.push({ operation: cancelled });
  await el("check").click();
  assert.equal(el("operation-message").textContent, "This operation has ended; it cannot be cancelled.");
  ctx.responses.push({ operation: cancelled });
  const sent = ctx.calls.length;
  await el("cancel").click();
  assert.deepEqual(ctx.calls.slice(sent).map((call) => call.method), ["GET"]);
  assert.match(el("operation-message").textContent, /already ended, so there is nothing to cancel\. Nothing was sent\./);

  // Stale tokens: explained, the next Cancel reads fresh ones and uses a new key.
  el("operation").value = B; el("operation").input();
  assert.equal(el("cancel").disabled, false);
  ctx.responses.push({ operation: op(B) }, fail(409, "operation_generation_stale"));
  await el("cancel").click();
  const staleKey = ctx.calls.at(-1).body.idempotency_key;
  assert.match(el("operation-message").textContent, /^HTTP 409: operation_generation_stale: x\nThe operation changed after its status was read[\s\S]*Select Check status/);
  ctx.responses.push({ operation: op(B, { operation_generation: 9 }) }, fail(503, "operation_store_unavailable"));
  await el("cancel").click();
  const unknown = ctx.calls.at(-1).body;
  assert.notEqual(unknown.idempotency_key, staleKey);
  assert.equal(unknown.expected_operation_generation, 9);
  assert.match(el("operation-message").textContent, /same button again/);
  // After an unknown outcome, Cancel re-reads the status but repeats the exact earlier cancel (same key, same tokens).
  const half = op(B, { phase: "cancelling", operation_generation: 10 });
  ctx.responses.push({ operation: half }, { action: "cancel", operation: half }, listed());
  await el("cancel").click();
  assert.deepEqual(plain(ctx.calls.at(-2).body), plain(unknown));
  assert.match(ctx.confirms.at(-1), /repeats your earlier cancel whose outcome is unknown/);
  assert.match(el("operation-message").textContent, /^Cancellation started but is not finished\. Select Cancel again to finish it\./);
  // ...and the cancel after that is new again.
  ctx.responses.push({ operation: half }, fail(409, "transition_unavailable"));
  await el("cancel").click();
  assert.notEqual(ctx.calls.at(-1).body.idempotency_key, unknown.idempotency_key);
  assert.equal(ctx.calls.at(-1).body.expected_operation_generation, 10);
  assert.match(el("operation-message").textContent, /That change is not possible for this operation now/);

  // The server seals a cancel before releasing the reservation; if the release fails, the exact earlier cancel finishes it.
  const E = "e".repeat(32), stored = keyIn(ROOT, `user:${ALICE}`, E);
  const releasing = op(E, { phase: "cancelled", operation_generation: 5, observed: { journal_steps: 3, uncertain_steps: 0, terminal: true, reserved: true } });
  const nowReleasedOf = (id) => op(id, { phase: "cancelled", operation_generation: 6, observed: { journal_steps: 4, uncertain_steps: 0, terminal: true, reserved: false } });
  el("operation").value = E; el("operation").input();
  // An answer naming another operation is an unknown outcome: the cancel stays saved.
  ctx.responses.push({ operation: op(E) }, { action: "cancel", operation: op(B) });
  await el("cancel").click();
  const sealed = plain(ctx.calls.at(-1).body);
  assert.match(el("operation-message").textContent, /did not confirm the cancel[\s\S]*same button again/);
  assert.deepEqual(JSON.parse(session.get(stored)).body, sealed);
  // The saved record holds only the operation, its owner, the API address, the path and the cancel body: no password or token.
  assert.deepEqual(Object.keys(JSON.parse(session.get(stored))).sort(), ["body", "operation", "path", "principal", "scope"]);
  assert.equal(JSON.parse(session.get(stored)).principal, `user:${ALICE}`);
  assert.equal(JSON.parse(session.get(stored)).scope, ROOT);
  ctx.responses.push({ operation: op(E) }, { action: "cancel", operation: releasing }, listed());
  await el("cancel").click();
  assert.deepEqual(plain(ctx.calls.at(-2).body), sealed);
  assert.equal(el("operation-message").textContent.split("\n")[0], `${RELEASE_PENDING} Select Cancel again to resend the same cancel and finish the release.`);
  assert.equal(el("operation-message").textContent.split("\n").at(-1), deploymentCancelScope(`user:${ALICE}`, ROOT));
  assert.equal(el("cancel").disabled, false);
  assert.match(el("operation-help").textContent, /Select Cancel to resend[\s\S]*for the account that sent it/);
  // The saved cancel survives losing the page's memory (this tab's sessionStorage keeps it).
  el("operation").value = B; el("operation").input(); el("operation").value = E; el("operation").input();
  ctx.responses.push({ operation: releasing });
  await el("check").click();
  assert.match(el("operation-message").textContent, /Select Cancel to resend your earlier cancel exactly[\s\S]*for the account that sent it/);
  const released = op(E, { phase: "cancelled", operation_generation: 6, observed: { journal_steps: 4, uncertain_steps: 0, terminal: true, reserved: false } });
  ctx.responses.push({ operation: releasing }, { action: "cancel", operation: released }, listed());
  await el("cancel").click();
  assert.match(ctx.confirms.at(-1), /^Finish cancelling operation e{32}\?/);
  assert.deepEqual(plain(ctx.calls.at(-2).body), sealed);
  assert.match(el("operation-message").textContent, /^Cancelled\./);
  assert.equal(session.has(stored), false);
  assert.equal(el("cancel").disabled, true);
  // Without the earlier cancel, Cancel sends a new one that quotes the status it reads first.
  const lost = "1".repeat(32), lostKey = keyIn(ROOT, `user:${ALICE}`, lost);
  const lostReleasing = { ...releasing, operation_id: lost, operation_generation: 7, lifecycle_generation: 3 };
  el("operation").value = lost; el("operation").input();
  ctx.responses.push({ operation: lostReleasing });
  await el("check").click();
  assert.equal(el("operation-message").textContent, RELEASE_LOST);
  assert.equal(el("cancel").disabled, false);
  assert.ok(el("operation-help").textContent.includes(RELEASE_LOST));
  // Declining the confirmation sends nothing.
  const declined = ctx.calls.length;
  ctx.answer = false;
  ctx.responses.push({ operation: lostReleasing });
  await el("cancel").click();
  ctx.answer = true;
  assert.deepEqual(ctx.calls.slice(declined).map((call) => call.method), ["GET"]);
  assert.equal(el("operation-message").textContent, "Nothing was sent.");
  assert.equal(session.has(lostKey), false);
  const freshSend = ctx.calls.length;
  ctx.responses.push({ operation: lostReleasing }, { action: "cancel", operation: { ...nowReleasedOf(lost), operation_generation: 8 } }, listed());
  await el("cancel").click();
  assert.deepEqual(ctx.calls.slice(freshSend).map((call) => call.method), ["GET", "POST", "GET"]);
  const fresh = ctx.calls[freshSend + 1];
  assert.equal(fresh.path, `/v1/admin/tenant-deployment-operations/${lost}/cancel`);
  assert.equal(fresh.body.expected_operation_generation, 7);
  assert.equal(fresh.body.expected_lifecycle_generation, 3);
  assert.notEqual(fresh.body.idempotency_key, sealed.idempotency_key);
  assert.match(ctx.confirms.at(-1), /^Finish cancelling operation 1{32}\?\n[\s\S]*sends a new cancel that quotes the status just read/);
  assert.doesNotMatch(ctx.confirms.at(-1), /same request key/);
  assert.match(el("operation-message").textContent, /^Cancelled\./);
  assert.equal(session.has(lostKey), false);
  assert.equal(el("cancel").disabled, true);
  // A server older than this recovery refuses the new cancel: the page says what to do and keeps no saved copy.
  const old = "2".repeat(32), oldKey = keyIn(ROOT, `user:${ALICE}`, old);
  el("operation").value = old; el("operation").input();
  ctx.responses.push({ operation: { ...releasing, operation_id: old } }, fail(409, "operation_terminal"));
  await el("cancel").click();
  assert.equal(ctx.calls.at(-1).method, "POST");
  assert.match(el("operation-message").textContent, /^HTTP 409: operation_terminal: x\n/);
  assert.ok(el("operation-message").textContent.includes(RELEASE_REFUSED));
  assert.equal(session.has(oldKey), false);
  // Cancel stays off for the refused operation instead of offering the same refusal again; Check status still works.
  assert.equal(el("cancel").disabled, true);
  assert.equal(el("operation-help").textContent, RELEASE_REFUSED);
  ctx.responses.push({ operation: { ...releasing, operation_id: old } });
  await el("check").click();
  assert.equal(el("operation-message").textContent, RELEASE_REFUSED);
  assert.equal(el("cancel").disabled, true);
  // Another operation is unaffected.
  el("operation").value = lost; el("operation").input();
  assert.equal(el("cancel").disabled, false);
  el("operation").value = old; el("operation").input();
  assert.equal(el("cancel").disabled, true);
  // A new cancel whose outcome is unknown is kept for this account and resent exactly.
  const unsure = "5".repeat(32), unsureKey = keyIn(ROOT, `user:${ALICE}`, unsure);
  el("operation").value = unsure; el("operation").input();
  ctx.responses.push({ operation: { ...releasing, operation_id: unsure } }, fail(503, "operation_store_unavailable"));
  await el("cancel").click();
  const unsureBody = plain(ctx.calls.at(-1).body);
  assert.match(el("operation-message").textContent, /same button again/);
  assert.deepEqual(JSON.parse(session.get(unsureKey)).body, unsureBody);
  ctx.responses.push({ operation: { ...releasing, operation_id: unsure } }, { action: "cancel", operation: nowReleasedOf(unsure) }, listed());
  await el("cancel").click();
  assert.deepEqual(plain(ctx.calls.at(-2).body), unsureBody);
  assert.match(ctx.confirms.at(-1), /resends your earlier cancel exactly, with the same request key/);
  assert.match(el("operation-message").textContent, /^Cancelled\./);
  assert.equal(session.has(unsureKey), false);
  // A saved new cancel the server never recorded (its first send failed) is refused on resend by an older server:
  // the page explains the refusal, drops the copy and keeps Cancel off.
  const unrecorded = "6".repeat(32), unrecordedKey = keyIn(ROOT, `user:${ALICE}`, unrecorded);
  el("operation").value = unrecorded; el("operation").input();
  ctx.responses.push({ operation: { ...releasing, operation_id: unrecorded } }, fail(503, "operation_store_unavailable"));
  await el("cancel").click();
  assert.ok(session.has(unrecordedKey));
  ctx.responses.push({ operation: { ...releasing, operation_id: unrecorded } }, fail(409, "operation_terminal"));
  await el("cancel").click();
  assert.match(ctx.confirms.at(-1), /resends your earlier cancel exactly/);
  assert.ok(el("operation-message").textContent.includes(RELEASE_REFUSED));
  assert.equal(session.has(unrecordedKey), false);
  assert.equal(el("cancel").disabled, true);
  // A malformed ID or status sends nothing further.
  el("operation").value = "nope"; el("operation").input();
  const quiet = ctx.calls.length;
  await el("cancel").click();
  await el("check").click();
  assert.equal(ctx.calls.length, quiet);
  assert.match(el("operation-message").textContent, /Operation ID must be the 32-character ID/);
  el("operation").value = B;
  ctx.responses.push({ operation: op(A) });
  await el("cancel").click();
  assert.deepEqual(ctx.calls.slice(quiet).map((call) => call.method), ["GET"]);
  assert.match(el("operation-message").textContent, /unexpected operation status/);

  // A saved cancel belongs to the verified account that sent it. Another account signed in to this tab can neither resend
  // nor delete it; leaving the page keeps it for a reload; signing out or the session ending deletes it; unknown records go.
  const F = "f".repeat(32), G = "9".repeat(32), H = "8".repeat(32), J = "7".repeat(32), K = "6".repeat(32), M = "4".repeat(32), N = "3".repeat(32);
  const keyOf = (who, id) => keyIn(ROOT, who, id);
  const stillReserved = (id) => op(id, { phase: "cancelled", operation_generation: 5, observed: { journal_steps: 3, uncertain_steps: 0, terminal: true, reserved: true } });
  const nowReleased = (id) => op(id, { phase: "cancelled", operation_generation: 6, observed: { journal_steps: 4, uncertain_steps: 0, terminal: true, reserved: false } });
  const signIn = (next, c = ctx, t = tools) => { c.caps = next; t.capabilities(next); };
  async function leaveReleasePending(id, c = ctx) {
    el("operation").value = id; el("operation").input();
    c.responses.push({ operation: op(id) }, { action: "cancel", operation: stillReserved(id) }, listed());
    await el("cancel").click();
    assert.equal(el("operation-message").textContent.split("\n")[0], `${RELEASE_PENDING} Select Cancel again to resend the same cancel and finish the release.`);
    return plain(c.calls.at(-2).body);
  }
  async function checkShows(id, c = ctx) {
    el("operation").value = id; el("operation").input();
    c.responses.push({ operation: stillReserved(id) });
    await el("check").click();
    return el("operation-message").textContent;
  }
  async function resend(id, c = ctx) {
    el("operation").value = id; el("operation").input();
    c.responses.push({ operation: stillReserved(id) }, { action: "cancel", operation: nowReleased(id) }, listed());
    await el("cancel").click();
    assert.match(c.confirms.at(-1), /^Finish cancelling operation/);
    assert.match(el("operation-message").textContent, /^Cancelled\./);
    return plain(c.calls.at(-2).body);
  }
  const aliceF = keyOf(`user:${ALICE}`, F);
  const aliceBody = await leaveReleasePending(F);
  assert.ok(session.has(aliceF));
  // Leaving the page (pagehide) keeps it for a reload.
  tools.disconnect({ sessionEnded: false });
  assert.ok(session.has(aliceF));
  // Bob signs in to the same tab: Alice's cancel is neither offered nor sent; declining his new cancel sends nothing.
  signIn(userCaps(BOB));
  assert.equal(await checkShows(F), RELEASE_LOST);
  assert.equal(el("cancel").disabled, false);
  const bobSent = ctx.calls.length;
  ctx.answer = false;
  ctx.responses.push({ operation: stillReserved(F) });
  await el("cancel").click();
  ctx.answer = true;
  assert.deepEqual(ctx.calls.slice(bobSent).map((call) => call.method), ["GET"]);
  assert.match(ctx.confirms.at(-1), /sends a new cancel that quotes the status just read/);
  assert.equal(el("operation-message").textContent, "Nothing was sent.");
  assert.ok(session.has(aliceF));
  // Bob's own new cancel uses a new key and, while its outcome is unknown, is saved under Bob; Alice's stays hers.
  const bobF = keyOf(`user:${BOB}`, F);
  ctx.responses.push({ operation: stillReserved(F) }, fail(503, "operation_store_unavailable"));
  await el("cancel").click();
  assert.notEqual(ctx.calls.at(-1).body.idempotency_key, aliceBody.idempotency_key);
  assert.deepEqual(JSON.parse(session.get(bobF)).body, plain(ctx.calls.at(-1).body));
  assert.deepEqual(JSON.parse(session.get(aliceF)).body, aliceBody);
  // Bob's own cancel is saved under Bob.
  const bobG = keyOf(`user:${BOB}`, G);
  await leaveReleasePending(G);
  assert.ok(session.has(bobG));
  // Bob's session ends (sign-out, or a rejected session): his cancel and every unknown record (an older page's key under no
  // API address, with or without an owner, or a damaged key) are deleted; Alice's cancel, another API address's cancels
  // and unrelated keys stay.
  const legacy = `${STORE}${F}`, legacyOwned = `${STORE}user:${BOB}.${F}`, damaged = keyOf("user:nope", F), elsewhere = keyIn(PROXIED, `user:${BOB}`, G);
  const legacyValue = JSON.stringify({ operation: F, path: `/v1/admin/tenant-deployment-operations/${F}/cancel`, body: aliceBody });
  session.set(legacy, legacyValue); session.set(legacyOwned, legacyValue); session.set(damaged, "{}"); session.set(elsewhere, session.get(bobG));
  session.set("vlite.other", "kept");
  tools.disconnect({ sessionEnded: true });
  assert.deepEqual([...session.keys()].sort(), [aliceF, elsewhere, "vlite.other"].sort());
  session.delete(elsewhere);
  // Any sign-in deletes unknown records as well, so an older page's unowned cancel is never used.
  session.set(legacy, legacyValue);
  // Alice signs in again (her session never ended): her saved cancel resends exactly and is then deleted.
  signIn(userCaps(ALICE));
  assert.equal(session.has(legacy), false);
  assert.match(await checkShows(F), /Select Cancel to resend your earlier cancel exactly/);
  assert.deepEqual(await resend(F), aliceBody);
  assert.equal(session.has(aliceF), false);
  // Signing out deletes her saved cancel: signed in again, she can no longer resend it.
  const aliceH = keyOf(`user:${ALICE}`, H);
  await leaveReleasePending(H);
  assert.ok(session.has(aliceH));
  tools.disconnect({ sessionEnded: true });
  assert.equal(session.has(aliceH), false);
  signIn(userCaps(ALICE));
  assert.equal(await checkShows(H), RELEASE_LOST);
  // Refresh session first clears the page (no options), then finds the session ended (401): that still deletes it.
  const aliceJ = keyOf(`user:${ALICE}`, J);
  await leaveReleasePending(J);
  tools.disconnect();
  assert.ok(session.has(aliceJ));
  tools.disconnect({ sessionEnded: true });
  assert.equal(session.has(aliceJ), false);
  // The bootstrap admin token is one account for everyone who holds it. A page load with no password session deletes
  // the accounts' cancels only; after a reload, signing in with the token again resends the token's cancel exactly.
  signIn(tokenCaps);
  const tokenK = keyOf("admin_token", K);
  const tokenBody = await leaveReleasePending(K);
  assert.ok(session.has(tokenK));
  assert.equal(el("operation-message").textContent.split("\n").at(-1), deploymentCancelScope("admin_token", ROOT));
  session.set(aliceF, session.get(tokenK));
  const aliceElsewhere = keyIn(PROXIED, `user:${ALICE}`, F);
  session.set(aliceElsewhere, session.get(tokenK));
  tools.forgetAccountCancels();
  assert.deepEqual([...session.keys()].sort(), [tokenK, aliceElsewhere, "vlite.other"].sort());
  session.delete(aliceElsewhere);
  tools.disconnect({ sessionEnded: false });
  signIn(tokenCaps);
  assert.deepEqual(await resend(K), tokenBody);
  assert.equal(session.has(tokenK), false);
  // A rejected token or a sign-out ends the token's session too, and deletes its cancels.
  await leaveReleasePending(M);
  assert.ok(session.has(keyOf("admin_token", M)));
  tools.disconnect({ sessionEnded: true });
  assert.equal(session.has(keyOf("admin_token", M)), false);
  // A role without a verified account (fails closed): the cancel is kept only in the page's memory, never saved, and says so.
  signIn({ role: "admin", features: {}, routes: [] });
  assert.deepEqual(disabled(), []);
  const keysBefore = [...session.keys()];
  const memoryBody = await leaveReleasePending(N);
  assert.deepEqual([...session.keys()], keysBefore);
  assert.equal(el("operation-message").textContent.split("\n").at(-1), deploymentCancelScope(null, ROOT));
  assert.deepEqual(await resend(N), memoryBody);
  session.delete("vlite.other");
  signIn(caps);

  // Gating: reader, tenant-scoped and open-access sign-ins get every control disabled with the reason, and clicks send nothing.
  for (const [locked, reason] of [
    [{ role: "read_only", features: {}, routes: [] }, /requires a super admin/],
    [{ role: "scoped", scope: { tenant: "t" }, features: {}, routes: [] }, /requires a super admin/],
    [{ role: "admin", identity: { kind: "open", label: "Open access" }, features: {}, routes: [] }, /needs configured admin authentication/],
  ]) {
    ctx.caps = locked;
    tools.capabilities(locked);
    assert.deepEqual(disabled(), [...FIELDS, ...BUTTONS]);
    assert.match(el("operation-help").textContent, reason);
    const count = ctx.calls.length;
    el("name").value = "acme"; el("writer-text").value = "{}"; el("init-text").value = "{}"; el("operation").value = A;
    for (const id of BUTTONS) await el(id).click();
    assert.equal(ctx.calls.length, count);
    assert.match(el("create-message").textContent, reason);
    assert.match(el("operation-message").textContent, reason);
  }
  assert.match(availability({ role: "read_only", features: {}, routes: [] }, true).tenantDeployments.reason, /HA gateway/);
  // The HA gateway refuses the submission itself; that is explained, not retried.
  ctx.caps = caps;
  tools.capabilities(caps);
  assert.deepEqual(disabled(), []);
  assert.equal(el("operation-status").textContent, "No operation selected.");
  el("readers-tenant").value = T; el("readers-mode").value = "absent";
  ctx.responses.push(fail(403, "gateway_tenant_required"));
  await el("readers").click();
  assert.match(el("readers-message").textContent, /HA gateway does not serve admin routes/);
  assert.doesNotMatch(el("readers-message").textContent, /same button again/);

  // Sign-out clears every form, message, choice and the operation box, and drops pending keys.
  el("name").value = "acme"; el("writer-file").files = [file("writer.json", [123, 125])]; el("writer-file").change();
  el("init-text").value = "{}"; el("retire-reason").value = "r"; el("readers-mode").value = "absent"; el("readers-present").hidden = true;
  el("readers-routing").value = "readers"; el("operation").value = A;
  ctx.responses.push(fail(503, "operation_store_unavailable"));
  await el("create").click();
  const pendingKey = ctx.calls.at(-1).body.idempotency_key;
  ctx.responses.push({ operation: op(A) });
  await el("check").click();
  tools.disconnect();
  assert.deepEqual(FIELDS.filter((id) => !["readers-mode", "readers-routing"].includes(id) && el(id).value !== ""), []);
  assert.deepEqual([el("readers-mode").value, el("readers-routing").value, el("readers-present").hidden, el("writer-file").files.length], ["present", "writer_only", false, 0]);
  assert.deepEqual(["create-message", "readers-message", "retire-message", "operation-message", "writer-note"].map((id) => el(id).textContent), ["", "", "", "", ""]);
  assert.deepEqual([el("operation-status").textContent, el("operation-facts").hidden], ["No operation selected.", true]);
  assert.deepEqual(disabled(), [...FIELDS, ...BUTTONS]);
  // After signing in again the same create is a new request.
  tools.capabilities(caps);
  el("name").value = "acme"; el("writer-file").files = [file("writer.json", [123, 125])]; el("init-text").value = "{}";
  ctx.responses.push(fail(503, "operation_store_unavailable"));
  await el("create").click();
  assert.notEqual(ctx.calls.at(-1).body.idempotency_key, pendingKey);
  // A late answer after sign-out is dropped.
  ctx.responses.push(() => { tools.disconnect(); return { duplicate: false, operation: op(A) }; });
  await el("create").click();
  assert.equal(el("create-message").textContent, "");
  assert.equal(el("operation-status").textContent, "No operation selected.");

  // ---- One site, two API addresses ----
  // sessionStorage belongs to the site (scheme, host and port) and the tab, not to one dashboard: the Docker image sends
  // its requests under /api, and one site can serve dashboards for two servers under two paths. A saved cancel is read,
  // resent and deleted only by a page that sends its requests to the same API address.
  for (const key of [...session.keys()]) if (key.startsWith(STORE)) session.delete(key);
  const cancelKeys = () => [...session.keys()].filter((key) => key.startsWith(STORE)).sort();
  // A page load: the same markup, a new page with its own apiBase, and nothing known about who is signed in yet.
  const page = (apiBase) => { reset(); timers.length = 0; const c = fakeCtx(); c.apiBase = apiBase; return { c, t: init(c) }; };
  const SHARED = "5".repeat(32), LATER = "2".repeat(32), TOKEN_OP = "c5".repeat(16), ALICE_OP = "d4".repeat(16);
  const rootKey = (who, id) => keyIn(ROOT, who, id), proxiedKey = (who, id) => keyIn(PROXIED, who, id);
  const aliceRoot = rootKey(`user:${ALICE}`, SHARED), aliceProxied = proxiedKey(`user:${ALICE}`, SHARED);
  // Dashboard A sends its requests to the site root. Alice leaves SHARED release-pending there; then the tab moves on.
  const A1 = page("");
  signIn(userCaps(ALICE), A1.c, A1.t);
  const rootBody = await leaveReleasePending(SHARED, A1.c);
  assert.deepEqual(cancelKeys(), [aliceRoot]);
  assert.equal(JSON.parse(session.get(aliceRoot)).scope, ROOT);
  assert.equal(el("operation-message").textContent.split("\n").at(-1), deploymentCancelScope(`user:${ALICE}`, ROOT));
  const rootSaved = session.get(aliceRoot);
  A1.t.disconnect({ sessionEnded: false });
  // Dashboard B, on the same site and in the same tab, sends its requests under /vlite-b/api. The same account checking
  // the same operation ID is not offered A's cancel; Cancel offers only a new cancel, and declining it sends nothing.
  const B1 = page("/vlite-b/api");
  signIn(userCaps(ALICE), B1.c, B1.t);
  assert.equal(await checkShows(SHARED, B1.c), RELEASE_LOST);
  assert.equal(el("cancel").disabled, false);
  const bSent = B1.c.calls.length;
  B1.c.answer = false;
  B1.c.responses.push({ operation: stillReserved(SHARED) });
  await el("cancel").click();
  B1.c.answer = true;
  assert.deepEqual(B1.c.calls.slice(bSent).map((call) => call.method), ["GET"]);
  assert.match(B1.c.confirms.at(-1), /sends a new cancel that quotes the status just read/);
  assert.equal(el("operation-message").textContent, "Nothing was sent.");
  // B's own cancel for the same operation ID and account is filed under B's address; A's saved copy is untouched.
  const proxiedBody = await leaveReleasePending(SHARED, B1.c);
  assert.notEqual(proxiedBody.idempotency_key, rootBody.idempotency_key);
  assert.deepEqual(cancelKeys(), [aliceRoot, aliceProxied].sort());
  assert.equal(JSON.parse(session.get(aliceProxied)).scope, PROXIED);
  assert.equal(el("operation-message").textContent.split("\n").at(-1), deploymentCancelScope(`user:${ALICE}`, PROXIED));
  assert.equal(session.get(aliceRoot), rootSaved);
  // Every key names the address in encoded form and nothing else: no query, fragment or user information.
  for (const key of cancelKeys()) assert.match(key, /^vlite\.tenant-deployment\.cancel\.http%3A%2F%2Fdash\.test(%2Fvlite-b%2Fapi)?\/user:[0-9a-f]{32}\.[0-9a-f]{32}$/);
  // Reloading B: the same account resends B's own cancel exactly, and only B's copy is deleted.
  B1.t.disconnect({ sessionEnded: false });
  const B2 = page("/vlite-b/api");
  signIn(userCaps(ALICE), B2.c, B2.t);
  assert.match(await checkShows(SHARED, B2.c), /Select Cancel to resend your earlier cancel exactly/);
  assert.deepEqual(await resend(SHARED, B2.c), proxiedBody);
  assert.deepEqual(cancelKeys(), [aliceRoot]);
  // Nothing B does deletes A's records, whatever their owner: B's sign-in, its account purge, its sign-out and a token
  // rejected on B leave A's saved cancels (and A's damaged key) alone, and B's own go. A key from an older page, filed
  // under no address, cannot be attributed and is deleted by any dashboard.
  const rootToken = rootKey("admin_token", LATER), rootDamaged = rootKey("user:nope", LATER), oldFormat = `${STORE}admin_token.${LATER}`;
  for (const key of [rootToken, rootDamaged, oldFormat]) session.set(key, rootSaved);
  signIn(userCaps(ALICE), B2.c, B2.t);
  assert.deepEqual(cancelKeys(), [aliceRoot, rootDamaged, rootToken].sort());
  await leaveReleasePending(LATER, B2.c);
  assert.ok(session.has(proxiedKey(`user:${ALICE}`, LATER)));
  B2.t.forgetAccountCancels();
  assert.deepEqual(cancelKeys(), [aliceRoot, rootDamaged, rootToken].sort());
  const proxiedToken = proxiedKey("admin_token", LATER), proxiedDamaged = proxiedKey("user:nope", LATER);
  session.set(proxiedToken, rootSaved); session.set(proxiedDamaged, rootSaved);
  B2.t.disconnect({ sessionEnded: true });
  assert.deepEqual(cancelKeys(), [aliceRoot, proxiedToken, rootDamaged, rootToken].sort());
  const B3 = page("/vlite-b/api");
  B3.t.disconnect();
  B3.t.disconnect({ sessionEnded: true, rejectedToken: true });
  assert.deepEqual(cancelKeys(), [aliceRoot, rootDamaged, rootToken].sort());
  // Reloading A: Alice resends A's cancel exactly; the damaged key under A's address is deleted at sign-in.
  const A2 = page("");
  signIn(userCaps(ALICE), A2.c, A2.t);
  assert.deepEqual(cancelKeys(), [aliceRoot, rootToken].sort());
  assert.match(await checkShows(SHARED, A2.c), /Select Cancel to resend your earlier cancel exactly/);
  assert.deepEqual(await resend(SHARED, A2.c), rootBody);
  assert.deepEqual(cancelKeys(), [rootToken]);
  session.delete(rootToken);

  // ---- A token rejected before the page knew who was signed in ----
  // Alice and the bootstrap admin token each leave a cancel release-pending on A, and the tab reloads.
  const aliceOp = rootKey(`user:${ALICE}`, ALICE_OP), tokenOp = rootKey("admin_token", TOKEN_OP);
  const aliceOpBody = await leaveReleasePending(ALICE_OP, A2.c);
  A2.t.disconnect({ sessionEnded: false });
  const A3 = page("");
  signIn(tokenCaps, A3.c, A3.t);
  await leaveReleasePending(TOKEN_OP, A3.c);
  A3.t.disconnect({ sessionEnded: false });
  assert.deepEqual(cancelKeys(), [aliceOp, tokenOp].sort());
  const elsewhereToken = proxiedKey("admin_token", TOKEN_OP), unknownRoot = rootKey("user:nope", TOKEN_OP), oldToken = `${STORE}admin_token.${TOKEN_OP}`;
  for (const key of [elsewhereToken, unknownRoot, oldToken]) session.set(key, session.get(tokenOp));
  // A fresh page: connect first clears the page (no options), then the typed token is refused (401) before
  // capabilities() ever ran. The token's cancels on this address go, with every unattributable key; Alice's cancel
  // (her password session was not what was refused) and the other address's records stay.
  const A4 = page("");
  A4.t.disconnect();
  assert.deepEqual(cancelKeys(), [aliceOp, elsewhereToken, oldToken, tokenOp, unknownRoot].sort());
  A4.t.disconnect({ sessionEnded: true, rejectedToken: true });
  assert.deepEqual(cancelKeys(), [aliceOp, elsewhereToken].sort());
  // Signed in with the right token, nothing is offered: the release is reported lost, and Cancel offers only a new cancel.
  signIn(tokenCaps, A4.c, A4.t);
  assert.equal(await checkShows(TOKEN_OP, A4.c), RELEASE_LOST);
  const tokenSent = A4.c.calls.length;
  A4.c.answer = false;
  A4.c.responses.push({ operation: stillReserved(TOKEN_OP) });
  await el("cancel").click();
  A4.c.answer = true;
  assert.deepEqual(A4.c.calls.slice(tokenSent).map((call) => call.method), ["GET"]);
  assert.match(A4.c.confirms.at(-1), /sends a new cancel that quotes the status just read/);
  // Alice, whose session never ended, still resends hers exactly after signing in again.
  signIn(userCaps(ALICE), A4.c, A4.t);
  assert.deepEqual(await resend(ALICE_OP, A4.c), aliceOpBody);
  assert.deepEqual(cancelKeys(), [elsewhereToken]);
  // A sign-out on a fresh page that never learned who was signed in deletes every unattributable key on this
  // address and the older pages' keys, but no account's cancel (it cannot tell whose session that sign-out ended).
  session.set(tokenOp, session.get(elsewhereToken)); session.set(aliceOp, session.get(elsewhereToken));
  session.set(unknownRoot, "{}"); session.set(oldToken, "{}"); session.set(`${STORE}${TOKEN_OP}`, "{}");
  const A5 = page("");
  A5.t.disconnect({ sessionEnded: true });
  assert.deepEqual(cancelKeys(), [aliceOp, elsewhereToken, tokenOp].sort());
  // The rejected-token purge needs exactly true; anything else is an ordinary session end.
  for (const notTrue of [false, "yes", 1]) {
    page("").t.disconnect({ sessionEnded: true, rejectedToken: notTrue });
    assert.ok(session.has(tokenOp), String(notTrue));
  }
  // Leaving the page keeps everything, even if a rejection is claimed.
  page("").t.disconnect({ sessionEnded: false, rejectedToken: true });
  assert.ok(session.has(tokenOp));
  for (const key of cancelKeys()) session.delete(key);

  // ---- An API address the page cannot establish ----
  // If apiBase holds user information, a query or a fragment (or does not resolve), nothing is saved, read or deleted
  // under any address; the cancel lives only in the page's memory and the page says so.
  for (const base of ["https://user:pw@api.test/v", "/vlite-b/api?key=secret"]) {
    const kept = [rootKey(`user:${ALICE}`, ALICE_OP), proxiedKey(`user:${ALICE}`, ALICE_OP)];
    for (const key of kept) session.set(key, rootSaved);
    const U = page(base);
    signIn(userCaps(ALICE), U.c, U.t);
    const memoryBody = await leaveReleasePending(ALICE_OP, U.c);
    assert.deepEqual(cancelKeys(), kept.sort(), base);
    assert.equal(el("operation-message").textContent.split("\n").at(-1), deploymentCancelScope(`user:${ALICE}`, null));
    assert.deepEqual(await resend(ALICE_OP, U.c), memoryBody);
    U.t.forgetAccountCancels();
    U.t.disconnect({ sessionEnded: true, rejectedToken: true });
    assert.deepEqual(cancelKeys(), kept.sort(), base);
    for (const key of kept) session.delete(key);
  }
}
