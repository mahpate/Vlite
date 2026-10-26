"use strict";
// Super-admin lifecycle tools: users (list, add, change, new password, delete), tenants (create,
// usage, settings, suspend/resume, delete), TLS reload, replication log export, and the
// display-only signed-in identity summary.
// Pure validators are exported for Node tests; init(ctx) only registers listeners.
(() => {
  const U64_MAX = 18446744073709551615n;
  const EXPORT_ROUTE = "/v1/admin/replication/export";
  const LIFECYCLE_ROUTE = "/v1/admin/tenants/{id}/usage";
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
  const count = (n) => Number.isSafeInteger(n) ? n : n === undefined || n === null ? "unknown" : `about ${n} (exceeds browser integer precision)`;
  const STATES = { active: "active", suspended: "suspended (API keys blocked)" };
  // Older servers omit state/counts; those fields are shown only when reported.
  function tenantView(item) {
    const view = { id: item?.id, name: item?.name, max_vectors: limitText(item?.max_vectors) };
    if (typeof item?.state === "string") view.state = STATES[item.state] ?? item.state;
    if (Number.isSafeInteger(item?.suspended_unix_ms)) view.suspended_since = new Date(item.suspended_unix_ms).toISOString();
    if (Number.isSafeInteger(item?.collections)) view.collections = item.collections;
    if (item?.vectors !== undefined) view.vectors = count(item.vectors);
    return view;
  }
  function tenantList(value) {
    if (!object(value) || !Array.isArray(value.items)) throw new Error("Unexpected tenant list response");
    return { count: value.items.length, tenants: value.items.map(tenantView) };
  }
  // Settings body: blank name keeps the current name; mode keep|set|unlimited controls the limit.
  function tenantSettings(name, mode, limit) {
    const body = {};
    if (typeof name === "string" && name !== "") body.name = tenantName(name);
    if (mode === "set") {
      const value = maxVectors(limit);
      if (value === null) throw new Error("Enter the new vector limit, or choose \"Remove the limit\"; no request sent");
      body.max_vectors = value;
    } else if (mode === "unlimited" || mode === "keep" || mode === undefined || mode === "") {
      if (typeof limit === "string" && limit.trim() !== "") throw new Error("A new limit is filled in, but the limit choice is not \"Set a new limit\". Choose it or clear the field; no request sent");
      if (mode === "unlimited") body.max_vectors = null;
    } else throw new Error("Unknown vector limit choice; no request sent");
    if (Object.keys(body).length === 0) throw new Error("Nothing to change: enter a new name and/or choose a new vector limit; no request sent");
    return body;
  }
  function tenantUsage(value) {
    if (!object(value) || !object(value.tenant) || !Array.isArray(value.collections)) throw new Error("Unexpected tenant usage response");
    const credentials = object(value.credentials) ? value.credentials : {};
    return {
      tenant: tenantView(value.tenant),
      vectors: count(value.vectors),
      vector_limit: limitText(value.max_vectors),
      remaining_vectors: value.max_vectors === null || value.max_vectors === undefined ? "unlimited" : count(value.remaining_vectors),
      over_limit: value.over_quota === true ? "yes: new vectors are refused until usage drops below the limit or the limit is raised" : "no",
      collections: value.collections.map((item) => ({ name: item?.name, stored_as: item?.physical_name, vectors: count(item?.vectors) })),
      api_keys: { active: credentials.active ?? 0, expired: credentials.expired ?? 0, revoked: credentials.revoked ?? 0 },
    };
  }
  // Mirrors the server: trimmed, 3..64 ASCII [a-z0-9._@-] starting with a letter or digit, stored lowercased.
  function username(value) {
    const text = typeof value === "string" ? value.trim() : "";
    if (!/^[A-Za-z0-9][A-Za-z0-9._@-]{2,63}$/.test(text)) throw new Error("Username must be 3 to 64 characters (letters, digits, '.', '_', '-' or '@') and start with a letter or digit; no request sent");
    return text.toLowerCase();
  }
  // Length and character rules only; the server also refuses predictable passwords (weak_password).
  function password(value, label = "Password") {
    if (typeof value !== "string" || value === "") throw new Error(`${label} is required; no request sent`);
    if (LONE_SURROGATE.test(value)) throw new Error(`${label} contains invalid Unicode; no request sent`);
    if (CONTROL.test(value)) throw new Error(`${label} must not contain control characters; no request sent`);
    const length = [...value].length;
    if (length < 15 || length > 128) throw new Error(`${label} must be 15 to 128 characters (it has ${length}); no request sent`);
    return value;
  }
  const ROLES = { super_admin: "Super admin", editor: "Editor", viewer: "Viewer" };
  function userRole(value) {
    if (!Object.hasOwn(ROLES, value)) throw new Error("Choose a role: Viewer, Editor or Super admin; no request sent");
    return value;
  }
  // Blank tenant means every collection; super admins always cover the whole server.
  function userTenant(value, role) {
    const text = typeof value === "string" ? value.trim() : "";
    if (text === "") return null;
    if (role === "super_admin") throw new Error("Super admins manage the whole server and cannot belong to a tenant. Clear the tenant ID or choose Editor or Viewer; no request sent");
    return tenantId(text);
  }
  function userId(value) {
    const text = typeof value === "string" ? value.trim() : "";
    if (!/^[0-9a-fA-F]{32}$/.test(text)) throw new Error("User ID must be the 32-character ID shown by List users; no request sent");
    return text.toLowerCase();
  }
  function userRequest(fields) {
    const role = userRole(fields?.role);
    const body = { username: username(fields?.username), password: password(fields?.password), role };
    const tenant = userTenant(fields?.tenant, role);
    if (tenant !== null) body.tenant_id = tenant;
    body.must_change_password = fields?.mustChange !== false;
    body.disabled = fields?.disabled === true;
    return body;
  }
  // Only fields that differ from the listed account are sent; an empty username keeps it.
  function userChanges(current, fields) {
    if (!object(current)) throw new Error("List users first, then enter the user's ID to load it; no request sent");
    const body = {};
    const name = typeof fields?.username === "string" && fields.username.trim() !== "" ? username(fields.username) : current.username;
    if (name !== current.username) body.username = name;
    const role = userRole(fields?.role);
    if (role !== current.role) body.role = role;
    const tenant = userTenant(fields?.tenant, role);
    if (tenant !== (current.tenant_id ?? null)) body.tenant_id = tenant;
    if (typeof fields?.disabled === "boolean" && fields.disabled !== current.disabled) body.disabled = fields.disabled;
    if (typeof fields?.mustChange === "boolean" && fields.mustChange !== current.must_change_password) body.must_change_password = fields.mustChange;
    if (Object.keys(body).length === 0) throw new Error("Nothing to change: edit a field first; no request sent");
    return body;
  }
  const tenantLabel = (item) => item?.tenant_id ? `${item.tenant_name ? `"${item.tenant_name}" ` : ""}(${item.tenant_id})` : null;
  function userView(item) {
    const view = {
      id: item?.id, username: item?.username, role: ROLES[item?.role] ?? item?.role_label ?? item?.role,
      access: item?.role === "super_admin" ? "whole server" : tenantLabel(item) ? `tenant ${tenantLabel(item)}` : "all collections",
      status: item?.disabled === true ? "disabled (cannot sign in)" : "active",
      must_change_password: item?.must_change_password === true ? "yes, at next sign-in" : "no",
    };
    if (Number.isSafeInteger(item?.active_sessions)) view.signed_in_sessions = item.active_sessions;
    if (item && "last_login_unix_ms" in item) view.last_sign_in = Number.isSafeInteger(item.last_login_unix_ms) ? new Date(item.last_login_unix_ms).toISOString() : "none since the server started";
    return view;
  }
  function userList(value) {
    if (!object(value) || !Array.isArray(value.items)) throw new Error("Unexpected user list response");
    return { count: value.items.length, ...(Number.isSafeInteger(value.capacity) ? { limit: value.capacity } : {}), users: value.items.map(userView) };
  }
  // Display-only summary of the connected credential. Authorization stays server-side.
  function identity(caps, connected = true) {
    if (!connected || !object(caps)) return { kind: "none", text: "Not signed in", detail: "Sign in to see who you are and what you can do." };
    const reported = object(caps.identity) && typeof caps.identity.label === "string" && caps.identity.label !== "" ? caps.identity : null;
    if (reported?.kind === "user") {
      const where = tenantLabel(reported) ? `inside tenant ${tenantLabel(reported)}` : "in every collection";
      const details = {
        super_admin: "Full administrator: can manage users, tenants, API keys, backups and server settings.",
        editor: `Can read, search and change data ${where}. Cannot administer the server.`,
        viewer: `Can read and search ${where}. Cannot change data or administer the server.`,
      };
      const detail = details[reported.account_role] ?? "Permissions are decided by the server for this account.";
      return { kind: "user", text: `Signed in as ${reported.label}`, detail: reported.must_change_password === true ? `${detail} Choose a new password to continue.` : detail };
    }
    const kind = reported && typeof reported.kind === "string" ? reported.kind
      : ({ admin: "super_admin", read_only: "read_only", scoped: "scoped" })[caps.role] ?? "unknown";
    const label = reported?.label ?? ({ super_admin: "Admin token", read_only: "Read-only token", scoped: "Scoped API key" })[kind] ?? `Role "${caps.role ?? "unknown"}"`;
    const scope = object(caps.scope) ? caps.scope : {};
    const list = (value) => Array.isArray(value) ? value.map(String) : [];
    const collections = list(scope.collections);
    const where = collections.includes("*") ? "all collections" : collections.length ? `collections ${collections.join(", ")}` : "no collections";
    const tenant = reported?.tenant_id ?? scope.tenant_id;
    const details = {
      open: "No admin token is configured, so anyone who can reach this server has full administrator access. Configure an admin token before sharing it.",
      super_admin: "Full administrator: can manage tenants, API keys, backups and server settings.",
      read_only: "Can read and search every collection. Cannot change data or administer the server.",
      scoped: `Can ${list(scope.operations).join(", ") || "do nothing"} on ${where}${tenant ? ` inside tenant ${reported?.tenant_name ? `"${reported.tenant_name}" ` : ""}(${tenant})` : ""}. Cannot administer the server.`,
    };
    const suffix = reported ? "" : " (this server does not report identity details)";
    return { kind, text: kind === "open" ? label : `Signed in as ${label}${suffix}`, detail: details[kind] ?? "Permissions are decided by the server for this token." };
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
    const offline = { allowed: false, reason: "Sign in first." };
    if (!connected || !object(caps)) return { users: offline, tenants: offline, lifecycle: offline, tls: offline, replication: offline };
    const admin = role === "admin";
    const notAdmin = (what) => `${what} requires a super admin: a super-admin account or the bootstrap admin token (current role: ${role ?? "unknown"}). Editors, viewers, read-only and scoped credentials cannot use it.`;
    const users = !admin ? { allowed: false, reason: notAdmin("User management") }
      : features.accounts !== true ? { allowed: false, reason: "User accounts are disabled on this server: they require admin token authentication plus an admin directory (VLITE_ADMIN_DIR) at startup." }
      : { allowed: true, reason: "" };
    const tenants = !admin ? { allowed: false, reason: notAdmin("Tenant management") }
      : features.tenants !== true ? { allowed: false, reason: "Tenants are disabled on this server: they require admin token authentication plus an admin directory (VLITE_ADMIN_DIR) at startup." }
      : { allowed: true, reason: "" };
    const lifecycle = !tenants.allowed ? tenants
      : features.tenant_lifecycle !== true && !routes.includes(LIFECYCLE_ROUTE)
        ? { allowed: false, reason: "This server version cannot show tenant usage, rename tenants, change limits, or suspend/resume tenants. Upgrade the server to use these controls; List, Create and Delete still work." }
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
    return { users, tenants, lifecycle, tls, replication };
  }

  const HINTS = {
    invalid_username: "Usernames are 3 to 64 characters (letters, digits, '.', '_', '-' or '@') and start with a letter or digit.",
    weak_password: "Choose a longer or less predictable password (15 to 128 characters) that does not contain the username or a common word. A few unrelated words work well.",
    username_taken: "Another account already uses this username (usernames ignore case). Choose a different one.",
    cannot_modify_self: "You cannot change, reset or delete your own account here. Use Change password at the top of Overview, or ask another super admin.",
    user_not_found: "No account has this ID (it may already be deleted). List users to refresh.",
    accounts_disabled: "User accounts are disabled: configure admin token authentication and VLITE_ADMIN_DIR, then restart.",
    accounts_require_upgrade: "Editor and viewer accounts need a format v4 database. Upgrade it offline with vlite-maintain upgrade, or add a super admin instead.",
    accounts_full: "The account limit (1024) is reached. Delete unused accounts first.",
    accounts_commit_indeterminate: "The change is in effect but its durability was not confirmed. List users before retrying; do not repeat blindly.",
    password_change_required: "Choose a new password first (top of Overview).",
    tenants_disabled: "Tenants are disabled: configure admin token authentication and VLITE_ADMIN_DIR, then restart.",
    tenant_not_found: "No live tenant has this ID (it may already be deleted). List tenants to refresh.",
    tenant_not_empty: "The tenant still owns collections (named t<tenant-id>__*). Drop those collections first with a credential scoped to the tenant or as admin, then delete again.",
    tenant_suspended: "This tenant is suspended: its API keys are refused and new keys cannot be issued for it. Resume the tenant first.",
    tenant_unavailable: "This tenant was deleted, so its API keys no longer work.",
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
      "tenant-usage", "tenant-new-name", "tenant-limit-mode", "tenant-new-limit", "tenant-save-settings", "tenant-suspend", "tenant-resume",
      "tls-reload", "tls-result", "replication-after", "replication-max-bytes", "replication-export", "replication-download", "replication-result",
      "user-list", "user-id", "user-username", "user-role", "user-tenant", "user-password", "user-must-change", "user-disabled",
      "user-create", "user-save", "user-reset-password", "user-delete", "user-result"];
    for (const name of controls) if (!$(name)) throw new Error(`admin markup is missing control #${name}`);
    const OUTPUTS = { users: "user-result", tenants: "tenant-result", tls: "tls-result", replication: "replication-result" };
    const TARGETS = { ...OUTPUTS, lifecycle: "tenant-result" };
    let epoch = 0;
    let segment = null;
    // Accounts from the last List users, by ID; Save sends only the fields that differ.
    let known = new Map();
    let gates = availability(null, false);
    const show = (id, value) => { $(id).textContent = typeof value === "string" ? value : JSON.stringify(value, null, 2); };
    const current = (mark) => mark === epoch && ctx.connected();
    const need = (area) => {
      gates = availability(ctx.capabilities(), ctx.connected());
      if (!gates[area].allowed) { show(TARGETS[area], gates[area].reason); throw new Error(gates[area].reason); }
    };
    // Every action captures the epoch; disconnect/reconnect bumps it so late results are dropped.
    const on = (name, area, action) => $(name).addEventListener("click", () => ctx.run(async () => {
      const mark = epoch;
      try { await action(mark); }
      catch (error) {
        if (!error?.stale && current(mark)) show(TARGETS[area], explain(error));
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
      if (!await ctx.confirm(`Delete tenant ${value}? The server refuses (HTTP 409 tenant_not_empty) while any collection named t${value}__* exists; drop those collections first. API keys scoped to this tenant stop working and its user accounts are removed. The dashboard cannot undo the deletion.`)) return;
      if (!current(mark)) return;
      const result = await ctx.api("DELETE", `/v1/admin/tenants/${value}`, undefined, { data: false });
      if (current(mark)) show("tenant-result", { tenant_id: value, ...(object(result) ? result : { result }) });
    });
    // Users: the form edits one account at a time. Entering a listed ID loads its fields.
    const userFields = () => ({
      username: $("user-username").value, password: $("user-password").value, role: $("user-role").value,
      tenant: $("user-tenant").value, mustChange: $("user-must-change").checked === true, disabled: $("user-disabled").checked === true,
    });
    const remember = (user) => { if (object(user) && typeof user.id === "string") known.set(user.id, user); };
    const fill = (user) => {
      $("user-username").value = user.username ?? "";
      $("user-role").value = Object.hasOwn(ROLES, user.role) ? user.role : "viewer";
      $("user-tenant").value = user.tenant_id ?? "";
      $("user-must-change").checked = user.must_change_password === true;
      $("user-disabled").checked = user.disabled === true;
      $("user-password").value = "";
    };
    const named = (id) => known.has(id) ? `"${known.get(id).username}" (${id})` : id;
    $("user-id").addEventListener("input", () => {
      const id = $("user-id").value.trim().toLowerCase();
      if (!known.has(id)) return;
      fill(known.get(id));
      show("user-result", { loaded: userView(known.get(id)), next: "Change the fields, then choose Save changes, Set new password or Delete user." });
    });
    on("user-list", "users", async (mark) => {
      need("users");
      const value = await ctx.api("GET", "/v1/admin/users", undefined, { data: false });
      if (!current(mark)) return;
      const view = userList(value);
      known = new Map();
      for (const item of value.items) remember(item);
      show("user-result", { ...view, next: "Enter a user's ID above to load and change it." });
    });
    on("user-create", "users", async (mark) => {
      need("users");
      const body = userRequest(userFields());
      const where = body.role === "super_admin" ? "the whole server" : body.tenant_id ? `tenant ${body.tenant_id}` : "every collection";
      if (!await ctx.confirm(`Add ${ROLES[body.role].toLowerCase()} "${body.username}" with access to ${where}?${body.must_change_password ? " They must choose a new password at their first sign-in." : ""}${body.disabled ? " The account starts disabled." : ""} Share the password with them privately.`)) return;
      if (!current(mark)) return;
      $("user-password").value = "";
      const value = await ctx.api("POST", "/v1/admin/users", body, { data: false });
      if (!current(mark)) return;
      remember(value?.user);
      if (value?.user?.id) $("user-id").value = value.user.id;
      show("user-result", { added: object(value?.user) ? userView(value.user) : value, next: "They can now sign in." });
    });
    on("user-save", "users", async (mark) => {
      need("users");
      const id = userId($("user-id").value);
      const body = userChanges(known.get(id), userFields());
      const LABELS = { username: "username", role: "role", tenant_id: "tenant", disabled: "account disabled", must_change_password: "new password at next sign-in" };
      const describe = (key, value) => key === "role" ? ROLES[value] : key === "tenant_id" ? value ?? "none (all collections)" : typeof value === "boolean" ? value ? "yes" : "no" : value;
      const changes = Object.entries(body).map(([key, value]) => `${LABELS[key]} → ${describe(key, value)}`).join("; ");
      if (!await ctx.confirm(`Update ${named(id)}: ${changes}? Their open sessions end and they sign in again.`)) return;
      if (!current(mark)) return;
      const value = await ctx.api("PUT", `/v1/admin/users/${id}`, body, { data: false });
      if (!current(mark)) return;
      remember(value?.user);
      show("user-result", { saved: object(value?.user) ? userView(value.user) : value, changed: Array.isArray(value?.changed) ? value.changed : [] });
    });
    on("user-reset-password", "users", async (mark) => {
      need("users");
      const id = userId($("user-id").value);
      const body = { password: password($("user-password").value, "New password"), must_change_password: $("user-must-change").checked === true };
      if (!await ctx.confirm(`Set a new password for ${named(id)}? Their open sessions end.${body.must_change_password ? " They must choose their own password at the next sign-in." : ""} Share the password with them privately.`)) return;
      if (!current(mark)) return;
      $("user-password").value = "";
      const value = await ctx.api("POST", `/v1/admin/users/${id}/password`, body, { data: false });
      if (!current(mark)) return;
      remember(value?.user);
      show("user-result", { password_set: object(value?.user) ? userView(value.user) : value });
    });
    on("user-delete", "users", async (mark) => {
      need("users");
      const id = userId($("user-id").value);
      if (!await ctx.confirm(`Delete user ${named(id)}? They are signed out everywhere and can no longer sign in. The dashboard cannot undo this.`)) return;
      if (!current(mark)) return;
      const value = await ctx.api("DELETE", `/v1/admin/users/${id}`, undefined, { data: false });
      if (!current(mark)) return;
      known.delete(id);
      show("user-result", { user_id: id, ...(object(value) ? value : { result: value }) });
    });
    // Lifecycle actions all target the tenant ID field and render the tenant the server returns.
    const lifecycle = async (mark, method, id, suffix, body, note) => {
      const value = await ctx.api(method, `/v1/admin/tenants/${id}/${suffix}`, body, { data: false });
      if (current(mark)) show("tenant-result", { tenant: object(value?.tenant) ? tenantView(value.tenant) : value, note });
      return current(mark);
    };
    on("tenant-usage", "lifecycle", async (mark) => {
      need("lifecycle");
      const id = tenantId($("tenant-id").value);
      const value = await ctx.api("GET", `/v1/admin/tenants/${id}/usage`, undefined, { data: false });
      if (current(mark)) show("tenant-result", tenantUsage(value));
    });
    on("tenant-save-settings", "lifecycle", async (mark) => {
      need("lifecycle");
      const id = tenantId($("tenant-id").value);
      const body = tenantSettings($("tenant-new-name").value, $("tenant-limit-mode").value, $("tenant-new-limit").value);
      const changes = [
        body.name !== undefined ? `rename it to "${body.name}"` : "",
        !("max_vectors" in body) ? "" : body.max_vectors === null ? "remove its vector limit" : `set its vector limit to ${body.max_vectors}`,
      ].filter(Boolean).join(" and ");
      if (!await ctx.confirm(`Update tenant ${id}: ${changes}? A limit below current usage keeps existing vectors but refuses new ones. The change is durable and audited.`)) return;
      if (!current(mark)) return;
      if (await lifecycle(mark, "PUT", id, "settings", body, "Saved. API keys bound to this tenant use the new limit immediately.")) {
        $("tenant-new-name").value = ""; $("tenant-limit-mode").value = "keep"; $("tenant-new-limit").value = "";
      }
    });
    on("tenant-suspend", "lifecycle", async (mark) => {
      need("lifecycle");
      const id = tenantId($("tenant-id").value);
      if (!await ctx.confirm(`Suspend tenant ${id}? Every API key bound to it is refused immediately (REST and gRPC) and no new keys can be issued for it. Its data is kept and the bootstrap admin can still read or clean it up. Resume undoes this.`)) return;
      if (!current(mark)) return;
      await lifecycle(mark, "POST", id, "suspend", undefined, "Suspended. Its API keys now receive tenant_suspended until you resume it.");
    });
    on("tenant-resume", "lifecycle", async (mark) => {
      need("lifecycle");
      const id = tenantId($("tenant-id").value);
      if (!await ctx.confirm(`Resume tenant ${id}? Its unexpired, unrevoked API keys work again immediately.`)) return;
      if (!current(mark)) return;
      await lifecycle(mark, "POST", id, "resume", undefined, "Active. Its unexpired, unrevoked API keys work again.");
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
      known = new Map();
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
        $("user-password").value = "";
        gate(true);
      },
      gate,
    };
  }

  const api = { init, utils: { tenantName, maxVectors, tenantId, tenantRequest, tenantPath, tenantList, tenantView, tenantSettings, tenantUsage, identity, exportPath, exportSegment, exportFilename, availability, explain, utf8Length,
    username, password, userRole, userTenant, userId, userRequest, userChanges, userView, userList } };
  if (typeof window !== "undefined") window.VLiteAdminTools = api;
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})();
