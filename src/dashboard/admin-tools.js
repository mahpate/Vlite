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
  // Account graph knobs exist only on graph builds; graph.js supplies the validators.
  const graphUtils = (tools) => tools ?? (typeof window !== "undefined" ? window.VLiteGraphTools?.utils : undefined);
  function userRequest(fields, tools) {
    const role = userRole(fields?.role);
    const body = { username: username(fields?.username), password: password(fields?.password), role };
    const tenant = userTenant(fields?.tenant, role);
    if (tenant !== null) body.tenant_id = tenant;
    body.must_change_password = fields?.mustChange !== false;
    body.disabled = fields?.disabled === true;
    const graph = graphUtils(tools);
    if (object(fields?.graph) && graph && role !== "super_admin") Object.assign(body, graph.accountGraphRequest(fields.graph, role, tenant !== null));
    return body;
  }
  // Only fields that differ from the listed account are sent; an empty username keeps it.
  function userChanges(current, fields, tools) {
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
    const graph = graphUtils(tools);
    if (object(fields?.graph) && graph) {
      // A super admin always has every graph, so only resets of earlier knobs are sent.
      const knobs = role === "super_admin" ? { graphs: "", access: "" } : fields.graph;
      Object.assign(body, graph.accountGraphChanges(current, knobs, role, tenant !== null));
    }
    if (Object.keys(body).length === 0) throw new Error("Nothing to change: edit a field first; no request sent");
    return body;
  }
  const tenantLabel = (item) => item?.tenant_id ? `${item.tenant_name ? `"${item.tenant_name}" ` : ""}(${item.tenant_id})` : null;
  const hasGraphKnobs = (item) => object(item) && Object.hasOwn(item, "graph_access");
  function userView(item, tools) {
    const view = {
      id: item?.id, username: item?.username, role: ROLES[item?.role] ?? item?.role_label ?? item?.role,
      access: item?.role === "super_admin" ? "whole server" : tenantLabel(item) ? `tenant ${tenantLabel(item)}` : "all collections",
      status: item?.disabled === true ? "disabled (cannot sign in)" : "active",
      must_change_password: item?.must_change_password === true ? "yes, at next sign-in" : "no",
    };
    const graph = graphUtils(tools);
    if (hasGraphKnobs(item) && graph) view.graphs = graph.accountGraphView(item);
    if (Number.isSafeInteger(item?.active_sessions)) view.signed_in_sessions = item.active_sessions;
    if (item && "last_login_unix_ms" in item) view.last_sign_in = Number.isSafeInteger(item.last_login_unix_ms) ? new Date(item.last_login_unix_ms).toISOString() : "none since the server started";
    return view;
  }
  function userList(value, tools) {
    if (!object(value) || !Array.isArray(value.items)) throw new Error("Unexpected user list response");
    return { count: value.items.length, ...(Number.isSafeInteger(value.capacity) ? { limit: value.capacity } : {}), users: value.items.map((item) => userView(item, tools)) };
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
    if (!connected || !object(caps)) return { users: offline, tenants: offline, lifecycle: offline, tls: offline, replication: offline, compaction: offline, cloudBackup: offline, autoFailover: offline, tenantDeployments: offline };
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
    const compaction = !admin ? { allowed: false, reason: notAdmin("File compaction") }
      : features.compaction !== true ? { allowed: false, reason: "This server does not offer file compaction: it needs the managed vlite-server runtime with an admin directory (VLITE_ADMIN_DIR), a durable single-file database and no gRPC listener." }
      : { allowed: true, reason: "" };
    const cloudBackup = !admin ? { allowed: false, reason: notAdmin("Cloud backup") }
      : features.cloud_backup !== true ? { allowed: false, reason: "This server does not offer cloud backup: it needs the managed vlite-server runtime with admin token authentication and a private admin directory (VLITE_ADMIN_DIR)." }
      : { allowed: true, reason: "" };
    // With no admin token the server answers status reads but refuses these admin routes with 401, which would sign the dashboard out.
    const open = object(caps.identity) && caps.identity.kind === "open";
    const needsToken = (what) => `${what} needs configured admin authentication. This server has no admin token, so it refuses these requests. Restart it with an admin token (VLITE_AUTH_TOKEN), then sign in.`;
    // Admin-only; whether this server can switch it is shown after Refresh status. Status still loads without an admin token.
    const autoFailover = !admin ? { allowed: false, reason: notAdmin("Automatic failover") }
      : open ? { allowed: true, reason: "", change: false, changeReason: needsToken("Turning automatic failover on or off") }
      : { allowed: true, reason: "", change: true, changeReason: "" };
    // Admin-only; there is no capability flag, so whether this server has it switched on shows after Load operations.
    const tenantDeployments = !admin ? { allowed: false, reason: `${notAdmin("Tenant deployments")} The HA gateway also refuses admin routes: sign in to the server directly.` }
      : open ? { allowed: false, reason: needsToken("Tenant deployments") }
      : { allowed: true, reason: "" };
    return { users, tenants, lifecycle, tls, replication, compaction, cloudBackup, autoFailover, tenantDeployments };
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
    tenant_not_empty: "The tenant still owns collections (named t<tenant-id>__*) or, on graph builds, graphs with that prefix. Drop those collections and graphs first with a credential scoped to the tenant or as admin, then delete again.",
    tenant_suspended: "This tenant is suspended: its API keys are refused and new keys cannot be issued for it. Resume the tenant first.",
    tenant_unavailable: "This tenant was deleted, so its API keys no longer work.",
    tenants_commit_indeterminate: "The change is in effect but its durability was not confirmed. List tenants before retrying; do not repeat blindly.",
    tls_disabled: "No file-backed TLS listener is configured; nothing was reloaded.",
    tls_reload_failed: "The TLS files on the server are invalid or unreadable. Nothing changed: the previous certificates remain active. Fix the files on the server host and retry.",
    settings_version_conflict: "The compaction settings changed in another session. The card now shows the latest status; review it and try again.",
    compaction_busy: "A compaction is already running. Wait for it to finish; the card updates on its own.",
    job_active: "A compaction is already running. Wait for it to finish; the card updates on its own.",
    job_finished: "That compaction already finished. Refresh to see its result.",
    selection_in_progress: "Too late to cancel: the server is already switching to the compacted file.",
    selection_committed: "Too late to cancel: the server already switched to the compacted file.",
    selection_outcome_unknown: "Too late to cancel: the switch result is being confirmed. Refresh in a moment.",
    idempotency_key_reused: "This start request was already used with different values. Refresh the status and start again.",
    acknowledgement_required: "Tick both statements before starting, or consent to dropping change history before turning on automatic compaction.",
    invalid_settings: "The server rejected these settings. Check the limits shown next to each field.",
    maintenance_in_progress: "Reads and writes are paused while compaction runs. Wait and refresh.",
    managed_runtime_required: "Compaction needs the managed vlite-server runtime with an admin directory (VLITE_ADMIN_DIR), a durable single-file database and no gRPC listener.",
    invalid_input: "The server rejected the input; nothing was changed.",
    invalid_query: "The server rejected the export parameters; nothing was exported.",
    invalid_id: "After sequence must be a canonical decimal u64.",
    admin_directory_disabled: "Cloud backup needs a private admin directory (VLITE_ADMIN_DIR) on the server. Configure it and restart.",
    cloud_backup_not_configured: "Cloud backup is not set up yet. Fill in the storage details and select Save cloud backup settings first.",
    cloud_backup_busy: "Another cloud backup operation is running. Wait for it to finish, then select Refresh status.",
    cloud_backup_policy_uncertain: "The saved settings could not be confirmed after a failure. Select Refresh status, or save the settings again, then retry.",
    cloud_backup_closed: "The server is shutting down. Try again after it restarts.",
    cloud_backup_requires_database_file: "Cloud backup needs the server to use a database file on disk, not an in-memory database. Nothing was uploaded.",
    encrypted_cloud_backup_unqualified: "Cloud backup of an encrypted database is not supported yet. Nothing was uploaded or restored.",
    memory_reservations_present: "Cloud backup is unavailable while agent memory is served by this server. Nothing was uploaded.",
    unqualified_filesystem: "The disk holding the database has not been qualified for safe cloud backup (for example some network file shares). Nothing was uploaded.",
    cloud_database_too_large: "The database is larger than the 1 GiB cloud backup limit. Nothing was uploaded.",
    cloud_prefix_owned: "Another VLite installation already owns this folder in the container. Choose a different folder.",
    cloud_store_failed: "Azure Storage refused or failed the request. Check the account, container and folder, and that the server's managed identity can use them, then try again.",
    cloud_store_unavailable: "Azure Storage could not be reached just now. Try again shortly.",
    cloud_publication_unknown: "The server could not confirm whether the backup finished uploading. Do not run it again blindly: it can create another backup. Select Load stored backups first.",
    cloud_publication_unresolved: "The server could not confirm whether the backup finished uploading. Do not run it again blindly: it can create another backup. Select Load stored backups first.",
    cloud_publication_lost: "An uploaded backup is no longer where the server left it. Select Refresh status and Load stored backups before doing anything else.",
    cloud_set_not_found: "No stored backup has this ID. Select Load stored backups to refresh the list.",
    cloud_set_unavailable: "This backup is not available: it is unfinished or being deleted. Choose a complete backup.",
    cloud_set_unpublished: "This backup is not finished uploading. Choose a complete backup.",
    cloud_set_corrupt: "The stored backup failed its integrity check. Do not rely on it; make a new backup.",
    cloud_seed_mode_deferred: "Restoring as a seed is not available yet.",
    restore_insufficient_space: "There is not enough free disk space in the server's admin directory for the restored copy. Free some space and try again.",
    cloud_pins_full: "This backup already has the maximum number of holds. Release one first.",
    cloud_pin_owned: "This hold belongs to a running operation, not an operator. Tick the override box only if that operation is no longer running.",
    cloud_pin_in_use: "This server is still running the operation that made this hold, so it cannot be released.",
    cloud_pin_not_found: "No hold with this ID exists on that backup. Select Load stored backups to refresh.",
    auto_failover_not_configured: "This server cannot switch automatic failover: only a writer started with an HA authority target (--serve-mode writer --authority-target-file) can.",
    auto_failover_unacknowledged: "Confirm the warning to turn automatic failover on. Nothing changed.",
    auto_failover_conflict: "Someone else changed automatic failover first. Select Refresh status to see the latest setting, then try again.",
    auto_failover_invalid: "The stored setting is damaged, so it cannot be turned on. Turn automatic failover off to replace it, then turn it on again.",
    auto_failover_unavailable: "The HA coordination store could not be reached. Nothing changed; try again shortly.",
    auto_failover_indeterminate: "The coordination store did not confirm the change. Select Refresh status to see whether it took effect before trying again.",
  };
  function explain(error) {
    const message = String(error?.message ?? error);
    // Off while agent memory is enabled: the message already says so; the 403 hint below would be wrong.
    if (error?.memoryMode) return message;
    const hint = HINTS[error?.code] || (error?.status === 403 ? "This credential is not permitted to perform this operation." : "");
    return hint ? `${message}\n${hint}` : message;
  }

  // ---- File compaction (Administration > Backup & recovery) ----
  const MIB = 1048576;
  const COMPACTION_MAX_MIB = 1024;
  const COMPACTION_PHASES = {
    preflight: "Checking that compaction can start",
    quiescing: "Pausing new requests and letting running ones finish",
    freezing: "Making a frozen copy of the database file",
    building: "Writing the compacted file",
    validating: "Checking the compacted file",
    ready_to_select: "Compacted file checked; about to switch to it",
    selecting: "Switching the server to the compacted file",
    selection_outcome_unknown: "Switch outcome unknown; the server needs a restart to recover",
    committed: "Switched to the compacted file",
    reopening: "Restarting the server on the new file",
    recovering_source: "Reopening the original file",
  };
  // Phases before the switch, where cancelling keeps the original file in use.
  const CANCELLABLE = ["preflight", "quiescing", "freezing", "building", "validating", "ready_to_select"];
  const TRIGGERS = { manual: "started from Compact now", scheduled: "automatic" };
  function formatBytes(value) {
    if (!Number.isSafeInteger(value) || value < 0) return "unknown";
    const exact = `${value.toLocaleString("en-US")} bytes`;
    const units = ["KiB", "MiB", "GiB", "TiB"];
    let n = value, unit = -1;
    while (n >= 1024 && unit < units.length - 1) { n /= 1024; unit++; }
    return unit < 0 ? exact : `${n.toFixed(n >= 100 ? 0 : 1)} ${units[unit]} (${exact})`;
  }
  const utcTime = (ms) => Number.isSafeInteger(ms) ? new Date(ms).toISOString().replace("T", " ").replace(/:\d\d\.\d{3}Z$/, " UTC") : "--";
  const mib = (bytes) => Number.isSafeInteger(bytes) ? String(Number((bytes / MIB).toFixed(3))) : "";
  // Server settings to form text; sizes in MiB, times in UTC.
  function compactionForm(settings) {
    const s = object(settings) ? settings : {};
    return {
      enabled: s.enabled === true, history: s.history_policy === "resnapshot_allowed",
      start: s.daily_start_utc ?? "", window: String(s.window_minutes ?? ""), maxRun: String(s.max_run_seconds ?? ""),
      minFile: mib(s.min_file_bytes), minGrowth: mib(s.min_growth_bytes), growthPercent: String(s.min_growth_percent ?? ""),
      cooldown: String(s.cooldown_hours ?? ""), maxSource: mib(s.max_source_bytes),
    };
  }
  // Form text to server settings. Bounds mirror the server; errors are per field, in plain words.
  function compactionSettings(fields, floorBytes = 64 * MIB) {
    const errors = {};
    const int = (key, min, max, message) => {
      const text = String(fields?.[key] ?? "").trim();
      const value = /^\d{1,9}$/.test(text) ? Number(text) : NaN;
      if (!(value >= min && value <= max)) { errors[key] = message; return null; }
      return value;
    };
    const size = (key, min, max, message) => {
      const text = String(fields?.[key] ?? "").trim();
      const bytes = /^\d{1,7}(\.\d{1,3})?$/.test(text) ? Math.round(Number(text) * MIB) : NaN;
      if (!(bytes >= min && bytes <= max)) { errors[key] = message; return null; }
      return bytes;
    };
    const start = String(fields?.start ?? "").trim();
    if (!/^([01]\d|2[0-3]):[0-5]\d$/.test(start)) errors.start = "Enter a 24-hour UTC time such as 02:00.";
    const window = int("window", 5, 120, "Enter whole minutes from 5 to 120.");
    const maxRun = int("maxRun", 60, 3600, "Enter whole seconds from 60 to 3600.");
    if (maxRun !== null && window !== null && maxRun + 60 > window * 60) errors.maxRun = `Must leave at least one minute of the ${window}-minute window: at most ${window * 60 - 60} seconds.`;
    const floorMib = mib(floorBytes) || "0";
    const maxSource = size("maxSource", Math.max(floorBytes, 1), COMPACTION_MAX_MIB * MIB, `Enter MiB above ${floorMib} and at most ${COMPACTION_MAX_MIB} (the qualified limit).`);
    const ceiling = maxSource ?? COMPACTION_MAX_MIB * MIB;
    const minFile = size("minFile", floorBytes, ceiling, `Enter MiB from ${floorMib} up to the largest file size.`);
    const minGrowth = size("minGrowth", floorBytes, ceiling, `Enter MiB from ${floorMib} up to the largest file size.`);
    const growthPercent = int("growthPercent", 10, 200, "Enter a whole percentage from 10 to 200.");
    const cooldown = int("cooldown", 24, 720, "Enter whole hours from 24 to 720.");
    const enabled = fields?.enabled === true, history = fields?.history === true;
    if (enabled && !history) errors.history = "Automatic compaction needs this consent.";
    if (Object.keys(errors).length) return { errors };
    return { errors, settings: {
      enabled, daily_start_utc: start, window_minutes: window, max_run_seconds: maxRun,
      min_file_bytes: minFile, min_growth_bytes: minGrowth, min_growth_percent: growthPercent,
      cooldown_hours: cooldown, max_source_bytes: maxSource,
      history_policy: history ? "resnapshot_allowed" : "unacknowledged",
    } };
  }
  function compactionOutcome(job) {
    if (!object(job)) return "";
    const switched = job.selection === "committed";
    const base = { succeeded: "Finished: the server now uses the compacted file.",
      cancelled: "Cancelled: the server kept using the original file.",
      skipped: "Skipped: the server kept using the original file.",
      failed: switched ? "Failed after switching to the compacted file." : "Failed: the server kept using the original file." }[job.outcome] ?? `Ended (${job.outcome}).`;
    const reasons = { no_material_savings: "the compacted file would not have been meaningfully smaller",
      maintenance_budget_exceeded: "the run would have exceeded its time limit", window_closed: "the automatic window closed",
      cancelled: "an administrator cancelled it", drain_timeout: "running requests did not finish in time", interrupted: "the server stopped during the run" };
    const why = job.code ? ` Reason: ${reasons[job.code] ?? job.code}.` : "";
    return `${base}${why}${job.action ? ` ${job.action}` : ""}`;
  }
  // Pure view model of GET /v1/admin/compaction.
  function compactionView(status) {
    if (!object(status)) return { headline: "Compaction status not loaded.", tone: "idle", facts: [], reasons: [], canStart: false, canCancel: false, watch: false };
    const reasons = (Array.isArray(status.reasons) ? status.reasons : [])
      .filter((r) => object(r) && !["job_active", "history_ack_required"].includes(r.code))
      .map((r) => String(r.message || r.code));
    const job = object(status.job) ? status.job : null;
    const recovery = object(status.recovery) ? status.recovery : {};
    const schedule = object(status.schedule) ? status.schedule : {};
    const settings = object(status.settings) ? status.settings : {};
    const file = object(status.file) ? status.file : {};
    const facts = [];
    let headline, tone;
    if (status.capability !== true) { headline = "Not available on this server."; tone = "blocked"; }
    else if (recovery.required === true) { headline = `Recovery needed (${recovery.code ?? "unknown"}). Data routes are closed until an operator restarts the server; the previous file and recovery copies are kept.`; tone = "blocked"; }
    else if (job) {
      headline = `In progress (${TRIGGERS[job.trigger] ?? job.trigger}): ${COMPACTION_PHASES[job.phase] ?? job.phase}.${job.cancel_requested ? " Cancellation requested; stopping at the next safe point." : ""}`;
      tone = "busy";
    } else if (status.fence === "maintenance") { headline = "The server is briefly paused for maintenance."; tone = "busy"; }
    else if (status.available !== true) { headline = "Compaction cannot run right now. See why below."; tone = "blocked"; }
    else { headline = "Ready. Compaction can run now."; tone = "ready"; }
    facts.push(["Database file", file.name ? `${file.name}, ${formatBytes(file.bytes)}` : formatBytes(file.bytes)]);
    if (job) {
      facts.push(["Started", utcTime(job.created_unix_ms)]);
      if (Number.isSafeInteger(job.before_bytes)) facts.push(["Size before", formatBytes(job.before_bytes)]);
      if (Number.isSafeInteger(job.after_bytes)) facts.push(["Compacted size", formatBytes(job.after_bytes)]);
    }
    const last = object(status.last_job) ? status.last_job : null;
    if (last) {
      facts.push(["Last run", `${utcTime(last.finished_unix_ms)}, ${TRIGGERS[last.trigger] ?? last.trigger}. ${compactionOutcome(last)}`]);
      if (Number.isSafeInteger(last.before_bytes) && Number.isSafeInteger(last.after_bytes)) {
        facts.push(["Last size change", `${formatBytes(last.before_bytes)} before, ${formatBytes(last.after_bytes)} after; saved ${formatBytes(last.saved_bytes ?? Math.max(0, last.before_bytes - last.after_bytes))}`]);
      }
    } else facts.push(["Last run", "Never"]);
    const auto = settings.enabled !== true ? "Off"
      : schedule.suspended === true ? `Suspended after ${schedule.failures ?? "repeated"} failed runs. Save the settings again to resume.`
      : `On, daily from ${settings.daily_start_utc} UTC for up to ${settings.window_minutes} minutes`;
    facts.push(["Automatic compaction", auto]);
    if (settings.enabled === true && schedule.suspended !== true) {
      facts.push(["Next window", schedule.in_window === true ? "Open now" : utcTime(schedule.next_window_unix_ms)]);
      if (Number.isSafeInteger(schedule.next_eligible_unix_ms) && schedule.next_eligible_unix_ms > (file.sampled_unix_ms ?? 0)) facts.push(["Waiting until", `${utcTime(schedule.next_eligible_unix_ms)} (cooldown)`]);
    }
    facts.push(["Growth since last compaction", Number.isSafeInteger(schedule.baseline_bytes)
      ? formatBytes(schedule.growth_bytes) : "Not measured yet: the baseline is set by the first compaction or the first automatic window"]);
    if (Number.isSafeInteger(recovery.retained_copies) && recovery.retained_copies > 0) facts.push(["Recovery copies kept", `${recovery.retained_copies}, ${formatBytes(recovery.retained_bytes)}`]);
    if (object(status.selection) && status.selection.candidate) facts.push(["Selected compacted file", String(status.selection.candidate)]);
    const canStart = status.capability === true && status.available === true && !job && status.fence === "open" && recovery.required !== true;
    const canCancel = !!job && CANCELLABLE.includes(job.phase) && job.cancel_requested !== true;
    const watch = !!job || status.fence === "maintenance";
    return { headline, tone, facts, reasons, canStart, canCancel, watch, job };
  }
  // RFC 4122 version 4, also on plain-HTTP origins where crypto.randomUUID is missing.
  function idempotencyKey(random = (bytes) => globalThis.crypto.getRandomValues(bytes)) {
    const b = random(new Uint8Array(16));
    b[6] = (b[6] & 0x0f) | 0x40; b[8] = (b[8] & 0x3f) | 0x80;
    const h = [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
    return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
  }

  // ---- Cloud backup (Administration > Backup & recovery) ----
  const CLOUD_FIELDS = { account: "cloud-backup-account", container: "cloud-backup-container", coordination: "cloud-backup-coordination",
    prefix: "cloud-backup-prefix", clientId: "cloud-backup-client-id", interval: "cloud-backup-interval", drillEvery: "cloud-backup-drill-every",
    keepSets: "cloud-backup-keep-sets", keepDays: "cloud-backup-keep-days", grace: "cloud-backup-grace" };
  const containerName = (name) => name.length >= 3 && name.length <= 63 && /^[a-z0-9]+(-[a-z0-9]+)*$/.test(name);
  const prefixName = (name) => name.length >= 1 && name.length <= 200 && /^[a-z0-9._/-]+$/.test(name) && name.endsWith("/") && !name.startsWith("/")
    && !name.includes("//") && !name.slice(0, -1).split("/").some((part) => part === "." || part === "..");
  function cloudInterval(seconds) {
    if (!Number.isFinite(seconds) || seconds <= 0) return "unknown";
    const unit = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
    if (seconds % 86400 === 0) return unit(seconds / 86400, "day");
    if (seconds % 3600 === 0) return unit(seconds / 3600, "hour");
    return unit(Number((seconds / 60).toFixed(2)), "minute");
  }
  // Server policy to form text; the interval is shown in minutes.
  function cloudBackupForm(policy) {
    const p = object(policy) ? policy : {};
    const t = object(p.target) ? p.target : {};
    const whole = (value, fallback) => String(Number.isSafeInteger(value) ? value : fallback);
    return {
      enabled: p.enabled === true, account: t.account ?? "", container: t.container ?? "", coordination: t.coordination_container ?? "",
      prefix: t.prefix ?? "", clientId: t.client_id ?? "",
      interval: Number.isFinite(p.interval_secs) ? String(Number((p.interval_secs / 60).toFixed(4))) : "60",
      drillEvery: whole(p.drill_every, 24), retention: p.retention_enabled === true,
      keepSets: whole(p.keep_sets, 3), keepDays: whole(p.keep_days, 30), grace: whole(p.orphan_grace_hours, 24),
    };
  }
  // Form text to the server policy. Bounds mirror the server; errors are per field, in plain words.
  // The page signs in with the server's managed identity only; a policy using anything else is left to the REST API.
  function cloudBackupPolicy(fields, previous) {
    const errors = {};
    const text = (key) => String(fields?.[key] ?? "").trim();
    const int = (key, min, max, message) => {
      const value = /^\d{1,9}$/.test(text(key)) ? Number(text(key)) : NaN;
      if (!(value >= min && value <= max)) { errors[key] = message; return null; }
      return value;
    };
    const auth = previous?.target?.auth;
    if (auth && auth !== "managed_identity") errors.account = "This server signs in with a test emulator key that this page cannot edit. Change it with the REST API.";
    const account = text("account"), container = text("container"), coordination = text("coordination"), prefix = text("prefix"), clientId = text("clientId");
    if (!errors.account && !/^[a-z0-9]{3,24}$/.test(account)) errors.account = "Enter 3 to 24 lowercase letters or digits.";
    if (!containerName(container)) errors.container = "Enter 3 to 63 lowercase letters, digits or single hyphens, not starting or ending with a hyphen.";
    if (coordination && !containerName(coordination)) errors.coordination = "Enter 3 to 63 lowercase letters, digits or single hyphens, or leave it empty.";
    else if (coordination && coordination === container) errors.coordination = "Must be a different container from the one above.";
    if (!prefixName(prefix)) errors.prefix = "Enter lowercase letters, digits, '.', '_', '-' and '/', ending with a slash (for example vlite/prod/), at most 200 characters, with no empty or dot-only parts.";
    if (clientId && !/^[0-9a-fA-F-]{36}$/.test(clientId)) errors.clientId = "Enter the 36-character identity client ID (letters a-f, digits and hyphens), or leave it empty.";
    const minutes = /^\d{1,6}(\.\d{1,4})?$/.test(text("interval")) ? Number(text("interval")) : NaN;
    if (!(minutes >= 5 && minutes <= 10080)) errors.interval = "Enter minutes from 5 to 10080 (one week).";
    const drillEvery = int("drillEvery", 0, 100, "Enter a whole number from 0 to 100.");
    const keepSets = int("keepSets", 3, 1000, "Enter a whole number from 3 to 1000.");
    const keepDays = int("keepDays", 30, 3650, "Enter whole days from 30 to 3650.");
    const grace = int("grace", 1, 720, "Enter whole hours from 1 to 720.");
    if (Object.keys(errors).length) return { errors };
    return { errors, policy: {
      target: { account, container, ...(coordination ? { coordination_container: coordination } : {}), prefix, auth: "managed_identity", ...(clientId ? { client_id: clientId } : {}) },
      enabled: fields?.enabled === true, interval_secs: Math.round(minutes * 60), retention_enabled: fields?.retention === true,
      keep_sets: keepSets, keep_days: keepDays, orphan_grace_hours: grace, drill_every: drillEvery,
      // The page has no archive controls; keep the saved archive settings rather than turning them off.
      ...(object(previous?.archive) ? { archive: previous.archive } : {}),
    } };
  }
  const cloudDrillText = (drill) => {
    if (!object(drill)) return "Never";
    const when = utcTime(drill.unix_ms);
    // Status reports carry error {code, message}; a stored backup record carries a detail string.
    const why = drill.error?.message ?? drill.error?.code ?? (typeof drill.detail === "string" && drill.detail ? drill.detail : "no reason recorded");
    return drill.ok === true ? `Passed, ${when}` : `Failed, ${when}: ${why}`;
  };
  function cloudCollectionText(collection) {
    if (!object(collection)) return "Never";
    const when = utcTime(collection.unix_ms);
    if (collection.ok !== true) return `Failed, ${when}: ${collection.error?.message ?? collection.error?.code ?? "no reason recorded"}`;
    const result = object(collection.result) ? collection.result : {};
    const count = (list) => Number.isSafeInteger(list?.count) ? list.count : Array.isArray(list) ? list.length : 0;
    const failed = count(result.failures);
    return `${when}: removed ${count(result.deleted)} old and ${count(result.orphans_deleted)} unfinished backups${collection.retention_enabled === true ? "" : " (automatic deletion of old backups is off)"}${failed ? `; ${failed} could not be removed` : ""}`;
  }
  // Pure view model of GET /v1/admin/cloud-backup.
  function cloudBackupView(state) {
    const idle = { headline: "Cloud backup status not loaded.", tone: "idle", facts: [], reasons: [], configured: false, uncertain: false, running: false, canRun: false };
    if (!object(state)) return idle;
    const policy = object(state.policy) ? state.policy : null;
    const target = object(policy?.target) ? policy.target : {};
    const status = object(state.status) ? state.status : {};
    const configured = state.configured === true && policy !== null;
    const uncertain = state.policy_uncertain === true;
    const running = state.running === true || object(status.job);
    const failedSince = Number.isSafeInteger(status.last_failure_unix_ms) && !(status.last_failure_unix_ms <= (status.last_success_unix_ms ?? 0));
    const reasons = [];
    let headline, tone;
    if (uncertain) { headline = "The saved settings could not be confirmed after a failure. Select Refresh status, or save the settings again, before anything else can run."; tone = "blocked"; }
    else if (!configured) { headline = "Not set up yet. Fill in the storage details below and save. Nothing is uploaded until you turn automatic backups on or select Back up now."; tone = "idle"; }
    else if (running) { headline = "A backup or clean-up is running now."; tone = "busy"; }
    else if (failedSince) { headline = "The latest backup failed. See what needs attention below."; tone = "blocked"; }
    else if (policy.enabled === true) { headline = `On: a backup is made every ${cloudInterval(policy.interval_secs)}.`; tone = "ready"; }
    else { headline = "Set up. Automatic backups are off; Back up now still works."; tone = "idle"; }
    if (uncertain) reasons.push("The server could not confirm which settings are saved. The settings shown are the last confirmed ones.");
    if (failedSince && object(status.last_error)) reasons.push(`The latest backup failed${status.last_error.code ? ` (${status.last_error.code})` : ""}: ${status.last_error.message ?? "no reason recorded"}`);
    if (Number.isSafeInteger(status.interrupted) && status.interrupted > 0) reasons.push(`${status.interrupted} earlier run(s) did not finish because the server stopped. Clean up now removes what they left behind.`);
    if (object(status.pending) && status.pending.set_id) reasons.push(`A backup upload (${status.pending.set_id}) has not been confirmed as complete yet.`);
    if (object(status.last_drill) && status.last_drill.ok === false) reasons.push(`The latest restore test failed: ${status.last_drill.error?.message ?? status.last_drill.error?.code ?? "no reason recorded"}`);
    if (object(status.last_collection) && status.last_collection.ok === false) reasons.push(`The latest clean-up failed: ${status.last_collection.error?.message ?? status.last_collection.error?.code ?? "no reason recorded"}`);
    if (Number.isSafeInteger(status.clock_reset_unix_ms)) reasons.push(`The server clock moved backwards at ${utcTime(status.clock_reset_unix_ms)}, so scheduled times were reset to the clock.`);
    const facts = [];
    if (configured) {
      facts.push(["Storage", `Account ${target.account}, container ${target.container}${target.coordination_container ? `, coordination container ${target.coordination_container}` : ""}, folder ${target.prefix}`]);
      facts.push(["Automatic backups", policy.enabled === true ? `On, every ${cloudInterval(policy.interval_secs)}` : "Off"]);
      if (policy.enabled === true) facts.push(["Next automatic backup", Number.isSafeInteger(state.next_run_unix_ms) ? utcTime(state.next_run_unix_ms) : "Not scheduled"]);
      facts.push(["Old backups", policy.retention_enabled === true
        ? `Deleted automatically, keeping the newest ${policy.keep_sets} and any younger than ${policy.keep_days} days`
        : "Never deleted automatically"]);
    }
    if (typeof state.installation_id === "string") facts.push(["This server's ID", state.installation_id]);
    if (configured || Number.isSafeInteger(status.runs)) {
      facts.push(["Last successful backup", Number.isSafeInteger(status.last_success_unix_ms)
        ? `${utcTime(status.last_success_unix_ms)}${status.last_set ? `, backup ${status.last_set}` : ""}` : "Never"]);
      facts.push(["Backups so far", `${status.published ?? 0} made, ${status.failures ?? 0} failed, ${status.skipped_busy ?? 0} skipped because the server was busy`]);
      facts.push(["Last restore test", cloudDrillText(status.last_drill)]);
      facts.push(["Last clean-up", cloudCollectionText(status.last_collection)]);
    }
    return { headline, tone, facts, reasons, configured, uncertain, running, canRun: configured && !uncertain && !running };
  }
  // Outcome text of POST /v1/admin/cloud-backup/run.
  function cloudRunOutcome(value) {
    if (!object(value)) return "The backup finished.";
    const parts = [`Backup ${value.set_id ?? "(unnamed)"} was uploaded.`];
    if (value.stopped_for_shutdown === true) parts.push("The server began shutting down, so the restore test and clean-up were skipped.");
    else {
      if (object(value.drill)) parts.push(`Restore test: ${cloudDrillText(value.drill)}.`);
      if (object(value.collection)) parts.push(`Clean-up: ${cloudCollectionText(value.collection)}.`);
    }
    return parts.join(" ");
  }
  // Outcome text of POST /v1/admin/cloud-backup/sets/{id}/restore: it writes a copy and never touches the live database.
  function cloudRestoreOutcome(value) {
    if (!object(value)) return "The restore finished.";
    const parts = [`Backup ${value.set_id ?? "(unnamed)"} was restored as a new copy named ${value.name ?? "(unnamed)"} in the server's admin directory. The live database was not changed.`];
    // Backups never carry secrets: the bundle only counts what must be issued again.
    const reissue = Number.isSafeInteger(value.bundle?.credentials) ? value.bundle.credentials : 0;
    if (reissue) parts.push(`${reissue} credential${reissue === 1 ? "" : "s"} must be issued again for the restored copy, because backups never contain secrets.`);
    if (value.pin_released === false) parts.push("The temporary hold this restore placed on the backup could not be released: load stored backups, choose it under Existing hold, tick the box below it and select Release hold.");
    parts.push("To use the copy: open Backups and restore above, select List snapshots, choose it under Restored copy for next startup, select Select restored copy for restart, then have an operator restart the server.");
    return parts.join(" ");
  }
  // View model of GET /v1/admin/cloud-backup/sets: newest first (set IDs start with their creation time).
  function cloudBackupSets(value) {
    const items = (Array.isArray(value?.sets) ? value.sets : []).filter((s) => object(s) && typeof s.set_id === "string").map((s) => {
      const record = object(s.record) ? s.record : null;
      const complete = record?.state === "published";
      const pins = (Array.isArray(record?.pins) ? record.pins : []).filter((p) => object(p) && typeof p.id === "string")
        .map((p) => ({ id: p.id, label: `${p.kind === "operation" ? "Held by a running operation" : "Held by an operator"}: ${p.reason ?? "no reason"}` }));
      const state = !record ? "unreadable" : complete ? "complete" : "unfinished";
      return { id: s.set_id, complete, pins, state, record, error: object(s.error) ? s.error : null, objects: s.data_objects,
        label: `${record ? utcTime(record.created_unix_ms) : "unknown time"}, ${state}${pins.length ? ", held" : ""} (${s.set_id})` };
    }).sort((a, b) => (a.id < b.id ? 1 : a.id > b.id ? -1 : 0));
    return { items, removing: Array.isArray(value?.tombstoned) ? value.tombstoned.length : 0 };
  }
  function cloudBackupSetFacts(item) {
    if (!object(item)) return [];
    const r = item.record;
    const facts = [["Backup ID", item.id], ["State", item.state === "complete" ? "Complete" : item.state === "unfinished" ? "Unfinished upload" : "Could not be read"]];
    if (r) {
      facts.push(["Created", utcTime(r.created_unix_ms)]);
      if (Number.isSafeInteger(r.verified_unix_ms)) facts.push(["Last verified", utcTime(r.verified_unix_ms)]);
      facts.push(["Restore test", cloudDrillText(r.last_drill)]);
      facts.push(["Holds", item.pins.length ? String(item.pins.length) : "None"]);
    }
    if (Number.isSafeInteger(item.objects)) facts.push(["Stored files", String(item.objects)]);
    if (item.error) facts.push(["Problem", item.error.message ?? item.error.code ?? "unknown"]);
    return facts;
  }

  // ---- Automatic failover (Administration > Backup & recovery) ----
  // View model of GET/PUT /v1/admin/auto-failover. It is unqualified (GATE-005 open), so "on" is never shown as healthy.
  function autoFailoverView(state) {
    const idle = { headline: "Automatic failover status not loaded.", tone: "idle", facts: [], available: false, enabled: false, invalid: false, canToggle: false };
    if (!object(state)) return idle;
    const available = state.available === true;
    const enabled = available && state.enabled === true;
    const invalid = available && state.invalid === true;
    let headline, tone;
    if (!available) { headline = "Not set up: automatic failover needs this server to be the writer with an HA authority target. Manual failover is unaffected."; tone = "idle"; }
    else if (invalid) { headline = "The stored setting is damaged, so supervisors treat it as off. Turn automatic failover off to replace it."; tone = "blocked"; }
    else if (enabled) { headline = "On (not yet proven safe): a supervisor that loses contact with the writer will fail over without review, and recent changes can be lost."; tone = "blocked"; }
    else { headline = "Off: failover happens only when a person runs it and reviews what could be lost."; tone = "idle"; }
    const facts = [];
    if (available) {
      facts.push(["Automatic failover", invalid ? "Damaged setting (treated as off)" : enabled ? "On" : "Off"]);
      facts.push(["Proven safe", `No: ${typeof state.gate === "string" ? state.gate : "GATE-005"} has not passed`]);
      if (Number.isSafeInteger(state.updated_unix_ms)) facts.push(["Last changed", utcTime(state.updated_unix_ms)]);
      if (typeof state.source === "string" && state.source) facts.push(["Changed by", state.source]);
      if (typeof state.revision === "string" && state.revision) facts.push(["Setting version", state.revision]);
    }
    return { headline, tone, facts, available, enabled, invalid, canToggle: available };
  }
  // PUT body for the toggle: turning on acknowledges the warning and guards against a concurrent change; turning off writes a fresh disabled revision.
  function autoFailoverRequest(state, enable) {
    if (!enable) return { enabled: false };
    const revision = object(state) && typeof state.revision === "string" && state.revision ? state.revision : null;
    return { enabled: true, acknowledge_unqualified: true, ...(revision ? { expected_revision: revision } : {}) };
  }

  // ---- Tenant deployments (Administration > Users): recorded operations, plus create, readers, retire and cancel (EPIC-011) ----
  // Nothing here deploys anything: the server only records intents while cloud execution is blocked (OD-002).
  const DEPLOY_OPERATIONS = "/v1/admin/tenant-deployment-operations";
  const DEPLOY_PAGE = 20;
  const DEPLOY_KINDS = { create: "Create tenant", set_readers: "Change reader replicas", migrate: "Move tenant", retire: "Retire tenant" };
  const DEPLOY_PHASES = {
    accepted: "Recorded, nothing reserved yet",
    reserved: "Reserved, waiting for cloud execution",
    cancelling: "Cancelling, not finished",
    activated: "Active",
    succeeded: "Finished",
    cancelled: "Cancelled",
    retired: "Retired: route removed, data kept",
  };
  const deployLabel = (map, value) => typeof value === "string" && Object.hasOwn(map, value) ? map[value] : `Unrecognized (${String(value ?? "missing")})`;
  // Refusals that mean "not available here", not a failure: shown as the panel status.
  const DEPLOY_BLOCKED = {
    tenant_deployments_not_configured: "Tenant deployments are off on this server. They need the server started with --tenant-deployments-file. Nothing is wrong with your sign-in.",
    read_only: "Read-only credentials cannot use tenant deployments. Sign in as a super admin.",
    forbidden_scope: "This credential cannot use admin routes. Sign in as a super admin.",
    gateway_tenant_required: "The HA gateway does not serve admin routes. Sign in to the server directly to use tenant deployments.",
    not_found: "This server does not offer tenant deployments (an older version, or the HA gateway). Sign in to an up-to-date server directly.",
  };
  // One plain explanation per refusal code the tenant deployment routes can answer (kept in sync with the server by tests).
  const DEPLOY_HINTS = {
    ...DEPLOY_BLOCKED,
    admin_auth_required: "Sign in with a super-admin account or the bootstrap admin token.",
    admin_dir_busy: "Another process (the server or a CLI command) holds this admin directory. Use either the server or the CLI, not both at once.",
    tenants_disabled: "Create needs tenants switched on: the server must run with admin token authentication and an admin directory (VLITE_ADMIN_DIR). Nothing was recorded.",
    invalid_query: "The page request was not accepted. Select Load operations to start again from the first page.",
    invalid_json: "The server could not read the request, so nothing was recorded. Reload the page and try again.",
    invalid_input: "The server may already have recorded the request, but it could not add the tenant record; the first line says why (for example, the tenant limit is reached). Resending the same request fails the same way until that is fixed.",
    internal_error: "The server hit an internal error. Select Load operations to see whether anything was recorded, and check the server log.",
    operation_store_unavailable: "The operation store could not be read or written right now. Try again shortly.",
    operation_indeterminate: "The operation store did not give a definite answer. Try again shortly.",
    operation_state_invalid: "A stored operation record is damaged. Nothing changed; check the server log.",
    operation_input_unavailable: "A profile file stored with this operation is missing or damaged. Do not edit operation records by hand; check the server log.",
    operation_not_found: "No operation with this ID is recorded on this server. Copy the ID from the list (Use this operation) and try again.",
    tenant_deployment_action_unknown: "This server does not offer that operation action. Nothing was recorded.",
    operation_request_invalid: "The server refused a field (the first line says which). Nothing was recorded. Fix the form and submit again.",
    operation_request_conflict: "This request key was already used for a different request, so nothing new was recorded. Submit again: the dashboard then uses a new key.",
    action_request_conflict: "This cancel's request key was already used with different details, so nothing was recorded. Select Check status, then Cancel again.",
    operation_generation_stale: "The operation changed after its status was read, so nothing was recorded. Select Check status to read it again, then Cancel again if you still want to.",
    tenant_id_conflict: "A different or deleted tenant already has the ID this request chose, so no tenant was added. The request itself stays recorded (Load operations lists it) but reserves nothing and does nothing. Select Create again: the dashboard then sends a new request, which picks a new ID.",
    tenants_commit_indeterminate: "The tenant record was written, but the server could not confirm it is safely stored. Select Load operations and refresh the Tenants list to see whether it is there, then select the same button again: it repeats the same request key, so the tenant is added at most once.",
    lifecycle_conflict: "Another change to this tenant happened at the same moment. Try again shortly.",
    pin_scope_unproven: "Reader replicas are not available yet (ITEM-008), so this cannot run. The recorded request stays as it is; Cancel withdraws it.",
    retirement_required: "This tenant is already active, so Cancel cannot undo it. Use Retire tenant instead (it keeps all data).",
    operation_terminal: "This operation has already finished or was withdrawn, so it cannot change any more. Nothing changed; select Check status to see how it ended.",
    operation_cancelled: "Cancellation was already requested for this operation, so it will not go ahead.",
    operation_not_terminal: "The operation has not finished yet. Check its status and wait, or cancel it.",
    operation_terminal_conflict: "The operation's stored outcome does not match the tenant's lifecycle. Nothing changed; check the server log.",
    operation_not_provisionable: "This operation does not hold a reservation of an unrouted tenant, so it cannot run. Nothing changed.",
    operation_journal_conflict: "The operation's step record changed at the same moment. Check its status, then try again.",
    operation_step_conflict: "The operation's recorded step does not match. Check its status, then try again.",
    prior_action_unknown: "An earlier step's outcome is not known yet. Nothing new runs until it is settled; check the server log.",
    cleanup_pending: "An earlier step still needs cleaning up before the operation can end. Do not edit records by hand; check the server log.",
    readers_in_progress: "A reader change for this tenant already started and must be finished first. The cancel may stay recorded on the server, but it changed nothing.",
    retirement_in_progress: "The retirement of this tenant already started and must be finished first. The cancel may stay recorded on the server, but it changed nothing.",
    withdrawal_capacity: "The server already keeps the maximum number of withdrawn requests (1,024), so it cannot withdraw this one. The cancel may stay recorded on the server, but it changed nothing.",
    transition_unavailable: "That change is not possible for this operation now. For example, a create that has not reserved its tenant yet cannot be cancelled. Nothing was recorded.",
    irreversible_ack_required: "This action is irreversible and needs an explicit acknowledgement. The dashboard does not offer it; nothing was recorded.",
    lifecycle_anchor_changed: "The tenant's lifecycle changed after this operation was planned. Nothing changed; check its status.",
    lifecycle_not_reserved: "The operation does not hold this tenant's reservation. Nothing changed; check its status.",
    lifecycle_reserved: "Another operation holds this tenant right now. Wait for it to finish or cancel it, then try again.",
    lifecycle_transition_unavailable: "The tenant's current routing does not allow this change. Nothing changed.",
    retirement_receipt_mismatch: "The receipt does not match the tenant's current route. Copy the receipt from the tenant's latest finished operation (Check status shows it), then submit a new retirement.",
    control_state_missing: "The control-state object named in --tenant-deployments-file does not exist yet. Initialize it first (see the HA guide).",
    control_state_unavailable: "The control-state store could not be reached. Try again shortly.",
    control_state_indeterminate: "The control-state store did not give a definite answer. Try again shortly.",
    control_state_conflict: "The control state changed at the same moment. Try again shortly.",
    control_state_invalid: "The stored control state is damaged. Nothing changed; check the server log.",
    control_state_stale: "The server read an older control state than it already saw. Try again shortly; if it keeps happening, check the server log.",
    control_state_regressed: "The stored control state went backwards. Nothing changed; check the server log.",
    control_state_generation_mismatch: "The stored control state does not match its generation. Nothing changed; check the server log.",
  };
  const DEPLOY_RETRY = "The server may already have recorded this, or part of it. Select the same button again without changing anything: it repeats the same request key, so the server records it at most once and finishes what it started. If you change anything first, it counts as a new request.";
  // invalid_input may also follow a recorded request, so the key is kept; but resending at once fails the same way.
  const DEPLOY_RETRY_AFTER_FIX = "Resending does not help until the cause is fixed. \"tenant capacity reached\" means the server already holds 1,024 tenant records; deleted tenants still count, so deleting one does not free a slot: report it to the server operator. This page keeps the request key: once the cause is fixed, select the same button again without changing the form. That repeats the same request key, so the request is recorded at most once and the server finishes what it started. Changing the form first sends a new request; the earlier one then stays listed under Load operations, reserving nothing.";
  const OPERATION_ID = /^[0-9a-f]{32}$/;
  function deploymentListPath(after = null, limit = DEPLOY_PAGE) {
    if (!Number.isInteger(limit) || limit < 1 || limit > 100) throw new Error("The page size must be 1 to 100.");
    if (after !== null && !OPERATION_ID.test(after)) throw new Error("The page position is not a valid operation ID. Select Load operations to start again.");
    return `${DEPLOY_OPERATIONS}?limit=${limit}${after === null ? "" : `&after=${after}`}`;
  }
  // Plain-language summary of what an operation asked for; server values are only ever shown as text.
  function deploymentRequest(kind, desired) {
    const d = object(desired) ? desired : {};
    if (kind === "create") return `Name ${typeof d.name === "string" ? d.name : "--"}; vector limit ${Number.isSafeInteger(d.max_vectors) ? d.max_vectors : "none"}`;
    if (kind === "set_readers") {
      const fleet = object(d.fleet) ? d.fleet : {};
      if (fleet.state === "absent") return "No reader replicas";
      if (fleet.state === "present") {
        const count = (value) => Number.isSafeInteger(value) && value >= 0 ? value : "?";
        return `${count(fleet.min_replicas)} to ${count(fleet.max_replicas)} reader replicas; reads go to ${deployLabel({ readers: "the readers", writer_only: "the writer only" }, fleet.read_routing)}`;
      }
      if (fleet.state !== undefined) return `Reader replicas: ${deployLabel({}, fleet.state)}`;
    }
    if (kind === "retire") return "Remove the route and stop the apps; all data is kept";
    if (kind === "migrate") return "Move the tenant to another writer";
    return "--";
  }
  function deploymentView(op) {
    const o = object(op) ? op : {};
    const observed = object(o.observed) ? o.observed : {};
    const text = (value) => typeof value === "string" && value ? value : "--";
    const steps = Number.isSafeInteger(observed.journal_steps)
      ? `${observed.journal_steps}${Number.isSafeInteger(observed.uncertain_steps) && observed.uncertain_steps > 0 ? ` (${observed.uncertain_steps} not confirmed)` : ""}`
      : "--";
    const facts = [
      ["Request", deploymentRequest(o.kind, o.desired)],
      ["Next step", text(o.next_step)],
      ["Requested", utcTime(o.created_unix_ms)],
      ["Operation ID", text(o.operation_id)],
      ["Tenant ID", text(o.tenant_id)],
      ["Recorded steps", steps],
    ];
    const receipt = typeof o.receipt_digest === "string" && /^[0-9a-f]{64}$/.test(o.receipt_digest) ? o.receipt_digest : null;
    if (receipt) facts.push(["Receipt (needed to retire this tenant)", receipt]);
    return {
      headline: `${deployLabel(DEPLOY_KINDS, o.kind)}: ${deployLabel(DEPLOY_PHASES, o.phase)}`,
      facts,
      operation: typeof o.operation_id === "string" && OPERATION_ID.test(o.operation_id) ? o.operation_id : null,
      tenant: typeof o.tenant_id === "string" && OPERATION_ID.test(o.tenant_id) ? o.tenant_id : null,
      receipt,
      kind: typeof o.kind === "string" ? o.kind : null,
      phase: typeof o.phase === "string" ? o.phase : null,
      terminal: deploymentTerminal(o),
      reserved: observed.reserved === true,
      releasePending: deploymentReleasePending(o),
    };
  }
  // `after` is the position this page was read from; the next position must move forward or paging could loop.
  function deploymentList(value, after = null) {
    if (!object(value) || !Array.isArray(value.operations) || !(value.next_after === null || (typeof value.next_after === "string" && OPERATION_ID.test(value.next_after)))
      || (after !== null && value.next_after !== null && value.next_after <= after)) {
      throw new Error("The server sent an unexpected tenant deployment list.");
    }
    return { items: value.operations.map(deploymentView), next: value.next_after };
  }
  // Any 404 means the route is not offered here (an older server or the HA gateway).
  const deploymentBlocked = (error) => error?.status === 404 ? DEPLOY_BLOCKED.not_found
    : typeof error?.code === "string" && error.code !== "not_found" && Object.hasOwn(DEPLOY_BLOCKED, error.code) ? DEPLOY_BLOCKED[error.code] : "";
  function deploymentExplain(error) {
    const message = String(error?.message ?? error);
    const code = typeof error?.code === "string" ? error.code : "";
    const hint = typeof error?.deployHint === "string" ? error.deployHint
      : code && code !== "not_found" && Object.hasOwn(DEPLOY_HINTS, code) ? DEPLOY_HINTS[code]
      : error?.status === 404 ? DEPLOY_BLOCKED.not_found
      : code ? "The dashboard has no explanation for this code. Select Load operations to see what is recorded; if it keeps failing, check the server log for this code."
      : "";
    const retry = error?.deployRetry !== true ? "" : code === "invalid_input" ? DEPLOY_RETRY_AFTER_FIX : DEPLOY_RETRY;
    return [message, hint, retry].filter(Boolean).join("\n");
  }

  // SHA-256 (FIPS 180-4) in plain JS: crypto.subtle only exists on HTTPS or localhost, and the
  // dashboard is also served over plain HTTP. Profiles are at most 1 MiB, so speed does not matter.
  const SHA_K = new Uint32Array([
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
  ]);
  const SHA_H = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
  // Lowercase hex SHA-256 of a string (as UTF-8) or of a Uint8Array.
  function sha256Hex(input) {
    const data = typeof input === "string" ? new TextEncoder().encode(input) : input;
    if (!(data instanceof Uint8Array)) throw new TypeError("sha256Hex needs a string or a Uint8Array");
    const length = data.length;
    const total = Math.ceil((length + 9) / 64) * 64;
    const block = new Uint8Array(total);
    block.set(data);
    block[length] = 0x80;
    const view = new DataView(block.buffer);
    view.setUint32(total - 8, Math.floor(length / 0x20000000));
    view.setUint32(total - 4, (length * 8) >>> 0);
    const h = Uint32Array.from(SHA_H);
    const w = new Uint32Array(64);
    const ror = (x, n) => (x >>> n) | (x << (32 - n));
    for (let offset = 0; offset < total; offset += 64) {
      for (let i = 0; i < 16; i++) w[i] = view.getUint32(offset + i * 4);
      for (let i = 16; i < 64; i++) {
        const s0 = ror(w[i - 15], 7) ^ ror(w[i - 15], 18) ^ (w[i - 15] >>> 3);
        const s1 = ror(w[i - 2], 17) ^ ror(w[i - 2], 19) ^ (w[i - 2] >>> 10);
        w[i] = (w[i - 16] + s0 + w[i - 7] + s1) | 0;
      }
      let [a, b, c, d, e, f, g, k] = h;
      for (let i = 0; i < 64; i++) {
        const t1 = (k + (ror(e, 6) ^ ror(e, 11) ^ ror(e, 25)) + ((e & f) ^ (~e & g)) + SHA_K[i] + w[i]) | 0;
        const t2 = ((ror(a, 2) ^ ror(a, 13) ^ ror(a, 22)) + ((a & b) ^ (a & c) ^ (b & c))) | 0;
        k = g; g = f; f = e; e = (d + t1) | 0; d = c; c = b; b = a; a = (t1 + t2) | 0;
      }
      h[0] += a; h[1] += b; h[2] += c; h[3] += d; h[4] += e; h[5] += f; h[6] += g; h[7] += k;
    }
    return Array.from(h, (x) => x.toString(16).padStart(8, "0")).join("");
  }

  // ---- Building tenant deployment submissions (pure, so tests can check the exact bodies) ----
  const DEPLOY_SUBMIT = "/v1/admin/tenant-deployments";
  const DEPLOY_CONTENT_MAX = 1048576;
  const DEPLOY_LIST_MAX = 64;
  const U32_MAX = 4294967295;
  const HEX64 = /^[0-9a-f]{64}$/;
  const NO_EFFECT = "This only records the request on this server. Cloud execution is switched off (OD-002), so nothing is deployed, routed, started or stopped in Azure.";
  const READERS_BLOCKED = "Reader replicas also wait for ITEM-008, so a reader fleet will not start either.";
  // Mirrors the server's text check: not empty, at most `max` UTF-8 bytes, no control characters.
  function deploymentText(value, label, max = 2048) {
    const text = typeof value === "string" ? value.trim() : "";
    if (text === "") throw new Error(`${label} is required; no request sent`);
    if (LONE_SURROGATE.test(text)) throw new Error(`${label} contains invalid Unicode; no request sent`);
    if (CONTROL.test(text)) throw new Error(`${label} must not contain tabs, line breaks or other control characters; no request sent`);
    const bytes = utf8Length(text);
    if (bytes > max) throw new Error(`${label} is ${bytes} bytes; at most ${max} are allowed. No request sent`);
    return text;
  }
  // One entry per line; blank lines are ignored and an empty box means an empty list.
  function deploymentLines(value, label) {
    const lines = String(value ?? "").split(/\r\n|\n|\r/).map((line) => line.trim()).filter(Boolean);
    if (lines.length > DEPLOY_LIST_MAX) throw new Error(`${label} has ${lines.length} lines; at most ${DEPLOY_LIST_MAX} are allowed. No request sent`);
    return lines.map((line, i) => deploymentText(line, `${label} line ${i + 1}`));
  }
  // The exact profile text the server stores, and the SHA-256 of its UTF-8 bytes.
  function deploymentContent(text, label) {
    if (typeof text !== "string" || text === "") throw new Error(`${label} is required: choose the file or paste its text; no request sent`);
    if (text.trim() === "") throw new Error(`${label} contains only spaces or blank lines: choose the file or paste its text; no request sent`);
    if (LONE_SURROGATE.test(text)) throw new Error(`${label} contains invalid Unicode; no request sent`);
    const bytes = new TextEncoder().encode(text);
    if (bytes.length > DEPLOY_CONTENT_MAX) throw new Error(`${label} is ${bytes.length} bytes; at most ${DEPLOY_CONTENT_MAX} (1 MiB) are allowed. No request sent`);
    return { text, bytes: bytes.length, digest: sha256Hex(bytes) };
  }
  // Each distinct text once: the server refuses repeated contents.
  const deploymentContents = (items) => [...new Map(items.map((item) => [item.digest, item.text])).values()];
  const contentLine = (label, item) => `${label}: ${item.bytes} bytes, SHA-256 ${item.digest}`;
  const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;
  function deploymentCreateSubmission(form) {
    const name = tenantName(form?.name);
    const limit = maxVectors(form?.maxVectors ?? "");
    if (limit === 0) throw new Error("Vector limit must be blank (no limit) or a whole number above 0; no request sent");
    const writer = deploymentContent(form?.writer, "Writer profile");
    const initialization = deploymentContent(form?.initialization, "Initialization settings");
    const resources = deploymentLines(form?.resources, "Azure resources");
    const secrets = deploymentLines(form?.secrets, "Secret versions");
    const request = {
      kind: "create", name, ...(limit === null ? {} : { max_vectors: limit }),
      writer_profile: writer.digest, initialization: initialization.digest, resources, secret_versions: secrets,
    };
    return {
      path: DEPLOY_SUBMIT, request, contents: deploymentContents([writer, initialization]),
      confirm: [
        `Record a request to create tenant "${name}" (vector limit ${limit === null ? "none" : limit})?`,
        "The server adds the tenant now under a new random ID, with no route, and reserves it for this request. Each new Create request adds another tenant record now, which counts toward the tenant limit and stays even if the request is later cancelled.",
        NO_EFFECT,
        contentLine("Writer profile", writer),
        contentLine("Initialization settings", initialization),
        `${plural(resources.length, "Azure resource", "Azure resources")} and ${plural(secrets.length, "secret version", "secret versions")} listed.`,
        "Cancel on this operation later releases the reservation; the tenant record itself stays.",
      ].join("\n"),
    };
  }
  function replicas(value, label, min) {
    const text = typeof value === "string" ? value.trim() : String(value ?? "");
    if (!/^[0-9]+$/.test(text) || Number(text) < min || Number(text) > U32_MAX) throw new Error(`${label} must be a whole number from ${min} to ${U32_MAX}; no request sent`);
    return Number(text);
  }
  function deploymentReadersSubmission(form) {
    const tenant = tenantId(form?.tenant);
    const path = `/v1/admin/tenant-deployments/${tenant}/readers`;
    const withdraw = "Cancel on this operation withdraws the request while it is still waiting.";
    if (form?.mode === "absent") {
      return {
        path, request: { kind: "set_readers", tenant_id: tenant, fleet: { state: "absent" } }, contents: [],
        confirm: [`Record a request to remove all reader replicas from tenant ${tenant}?`, NO_EFFECT, withdraw].join("\n"),
      };
    }
    if (form?.mode !== "present") throw new Error("Choose whether the tenant should have reader replicas; no request sent");
    const min = replicas(form?.min, "Minimum readers", 0);
    const max = replicas(form?.max, "Maximum readers", 1);
    if (min > max) throw new Error("Minimum readers must not be more than maximum readers; no request sent");
    const routing = form?.routing ?? "writer_only";
    if (routing !== "writer_only" && routing !== "readers") throw new Error("Choose where reads go; no request sent");
    const reader = deploymentContent(form?.reader, "Reader profile");
    const scaling = deploymentContent(form?.scaling, "Scaling rule");
    return {
      path,
      request: {
        kind: "set_readers", tenant_id: tenant,
        fleet: { state: "present", min_replicas: min, max_replicas: max, reader_profile: reader.digest, scaling_rule: scaling.digest, read_routing: routing },
      },
      contents: deploymentContents([reader, scaling]),
      confirm: [
        `Record a request for ${min} to ${max} reader replicas on tenant ${tenant}, with reads going to ${routing === "readers" ? "the readers" : "the writer only"}?`,
        NO_EFFECT, READERS_BLOCKED,
        contentLine("Reader profile", reader),
        contentLine("Scaling rule", scaling),
        withdraw,
      ].join("\n"),
    };
  }
  function deploymentRetireSubmission(form) {
    const tenant = tenantId(form?.tenant);
    const receipt = typeof form?.receipt === "string" ? form.receipt.trim().toLowerCase() : "";
    if (!HEX64.test(receipt)) throw new Error("Receipt must be the 64-character receipt shown on the tenant's latest finished operation; no request sent");
    const reason = deploymentText(form?.reason, "Reason", 512);
    return {
      path: `/v1/admin/tenant-deployments/${tenant}/retire`,
      request: { kind: "retire", tenant_id: tenant, expected_receipt: receipt, reason, data_disposition: "retain" },
      contents: [],
      confirm: [
        `Record a request to retire tenant ${tenant}?`,
        "When it runs, retiring removes the tenant's route and stops its apps. All data is kept.",
        NO_EFFECT,
        `Reason: ${reason}`,
        "Cancel on this operation withdraws the request while it is still waiting.",
      ].join("\n"),
    };
  }
  const deploymentTerminal = (op) => object(op) && (["succeeded", "cancelled", "retired"].includes(op.phase) || op.observed?.terminal === true);
  // Cancel seals the outcome first and then releases the tenant; if the release failed, an exact resend of
  // that same cancel (same key and body, same signed-in account) or a new cancel quoting the current status finishes it.
  const deploymentReleasePending = (op) => object(op) && op.phase === "cancelled" && object(op.observed) && op.observed.reserved === true;
  // The cancel body quotes the tokens from the status just read, so a change in between is refused, not overridden.
  function deploymentCancelRequest(op, key) {
    if (!object(op) || typeof op.operation_id !== "string" || !OPERATION_ID.test(op.operation_id)
      || !Number.isSafeInteger(op.operation_generation) || !Number.isSafeInteger(op.lifecycle_generation)) {
      throw new Error("The server sent an unexpected operation status. Select Check status to read it again; nothing was sent.");
    }
    return {
      path: `/v1/admin/tenant-deployment-operations/${op.operation_id}/cancel`,
      body: {
        idempotency_key: key,
        expected_operation_generation: op.operation_generation,
        expected_lifecycle_generation: op.lifecycle_generation,
        ...(typeof op.receipt_digest === "string" ? { expected_receipt: op.receipt_digest } : {}),
      },
    };
  }
  const CANCEL_EFFECT = {
    accepted: "It is recorded but nothing is reserved yet, so cancelling withdraws it for good.",
    reserved: "It holds the tenant's reservation; cancelling releases it. For a create, the tenant record stays.",
    cancelling: "Cancellation already started; this finishes it.",
  };
  function deploymentCancelConfirm(op, retrying) {
    const view = deploymentView(op);
    return [
      `Cancel operation ${view.operation}?`,
      `It is: ${deployLabel(DEPLOY_KINDS, op.kind)} for tenant ${view.tenant ?? "--"}, now ${deployLabel(DEPLOY_PHASES, op.phase)}.`,
      CANCEL_EFFECT[op.phase] ?? "The server decides whether it can still be cancelled.",
      "This only records the cancellation on this server. Cloud execution is switched off (OD-002), so nothing is deployed, routed, started or stopped in Azure.",
      ...(retrying ? ["This repeats your earlier cancel whose outcome is unknown, with the same request key, so it counts once."] : []),
    ].join("\n");
  }
  const RELEASE_PENDING = "This operation is cancelled, but the server has not released the tenant's reservation yet (the release step failed after the cancel was recorded).";
  function deploymentReleaseConfirm(op, resending) {
    return [
      `Finish cancelling operation ${deploymentView(op).operation}?`,
      RELEASE_PENDING,
      resending ? "This resends your earlier cancel exactly, with the same request key, so the server only finishes releasing the reservation."
        : "This sends a new cancel that quotes the status just read, so the server only finishes releasing the reservation. If the operation changes first, the server refuses it and nothing changes.",
      "Cloud execution is switched off (OD-002), so nothing in Azure is touched.",
    ].join("\n");
  }
  // Without a saved cancel for this account, Cancel sends a new one quoting the status it reads first.
  const RELEASE_LOST = `${RELEASE_PENDING} This browser tab has no saved copy of the earlier cancel for the account signed in now under this dashboard's API address (a saved cancel stays only in the tab that sent it, only for the account that sent it, and only until that account signs out or its session ends). Select Cancel to send a new cancel instead: it quotes the status read just before it is sent, and the server then finishes releasing the reservation.`;
  // A server that predates this recovery refuses a new cancel once the operation has ended.
  const RELEASE_REFUSED = "The server refused a new cancel for this operation. Select Check status: if the reservation is now released, nothing more is needed. If it is still held, this server is older than the dashboard and accepts only an exact resend of the earlier cancel, which this tab does not have: report the operation ID to the server operator.";
  // Saved cancels (sessionStorage) are filed by API address, then owner:
  // TD_CANCEL_STORE + encodeURIComponent(address) + "/" + principal + "." + operation ID.
  const TD_CANCEL_STORE = "vlite.tenant-deployment.cancel.";
  // The API address the page's requests go to: `${apiBase}/v1/...` resolved against the page, as fetch resolves it,
  // minus the "/v1/". sessionStorage is shared by every page of a site in a tab, and one site can serve dashboards for
  // several servers under different paths (the Docker image proxies its API at /api), so this, not the server's role
  // or a guess about prefixes, is what keeps their saved cancels apart. Null when it cannot be established, or when the
  // base carries credentials, a query or a fragment (never put in a key): then nothing is saved, read or deleted.
  function deploymentApiScope(apiBase, pageUrl) {
    if (typeof apiBase !== "string" || typeof pageUrl !== "string") return null;
    let url;
    try { url = new URL(`${apiBase}/v1/`, pageUrl); } catch { return null; }
    if ((url.protocol !== "http:" && url.protocol !== "https:") || url.username || url.password || url.search || url.hash
      || !url.pathname.endsWith("/v1/")) return null;
    return url.origin + url.pathname.slice(0, -"/v1/".length);
  }
  // An owner is the account the server verified for the sign-in, named as the server's actor is: a user account by
  // its ID, or the bootstrap admin token. A role alone, open access or any other identity is no account (null).
  const TD_PRINCIPAL = /^(user:[0-9a-f]{32}|admin_token)$/;
  function deploymentPrincipal(caps) {
    if (!object(caps) || caps.role !== "admin" || !object(caps.identity)) return null;
    const who = caps.identity;
    if (who.kind === "user") return who.account_role === "super_admin" && typeof who.user_id === "string" && /^[0-9a-f]{32}$/.test(who.user_id) ? `user:${who.user_id}` : null;
    return who.kind === "super_admin" ? "admin_token" : null;
  }
  // The owner named by a storage key, for a page whose API address is `scope`: undefined for keys this page does not
  // own (not a cancel key, or one filed under another API address, which is never read or deleted here); null for a
  // cancel key whose owner cannot be read (an older page's key filed under no address, or a damaged key under this
  // address), which is deleted, never used. A page without an address owns no key but still deletes older pages' keys.
  function deploymentCancelOwner(key, scope) {
    if (typeof key !== "string" || !key.startsWith(TD_CANCEL_STORE)) return undefined;
    const rest = key.slice(TD_CANCEL_STORE.length), slash = rest.indexOf("/");
    if (slash <= 0) return null;
    if (typeof scope !== "string" || rest.slice(0, slash) !== encodeURIComponent(scope)) return undefined;
    const own = rest.slice(slash + 1), dot = own.lastIndexOf(".");
    return dot > 0 && TD_PRINCIPAL.test(own.slice(0, dot)) && OPERATION_ID.test(own.slice(dot + 1)) ? own.slice(0, dot) : null;
  }
  // What the page says about how far a cancel it keeps reaches, for its owner and the page's API address.
  function deploymentCancelScope(principal, scope) {
    if (typeof principal !== "string" || !TD_PRINCIPAL.test(principal)) return "The server did not identify your account, so that cancel is not saved: it is kept only while this operation stays selected on this page, and a reload, a sign-out or another sign-in loses it.";
    if (typeof scope !== "string" || scope === "") return "The dashboard could not tell which API address it uses, so that cancel is not saved: it is kept only while this operation stays selected on this page, and a reload, a sign-out or another sign-in loses it.";
    return [
      "This tab saves that cancel (the operation ID, its request key and the values it quoted; never a password or token) for the account that sent it.",
      "After a reload, the same account signed in again can resend it, as long as its session did not end. Signing out, or the session ending, deletes it, so finish the release first.",
      "Another account signed in to this tab cannot resend it, but until it is deleted the saved copy is plain text that anyone using this tab can read.",
      "It is filed under the API address this dashboard uses; a dashboard on this site that uses another API address neither resends nor deletes it.",
      ...(principal === "admin_token" ? ["Everyone signed in with the bootstrap admin token counts as one account, and a rejected token counts as the session ending."] : []),
    ].join(" ");
  }
  // A saved cancel is used only by its owner under the API address it was sent to, and only if it is exactly the shape
  // this page sends for that operation.
  function deploymentSavedCancel(value, id, principal, scope) {
    if (typeof principal !== "string" || !TD_PRINCIPAL.test(principal) || typeof scope !== "string" || scope === "") return null;
    if (!object(value) || value.principal !== principal || value.scope !== scope || value.operation !== id || value.path !== `/v1/admin/tenant-deployment-operations/${id}/cancel`) return null;
    const body = value.body;
    if (!object(body) || typeof body.idempotency_key !== "string" || body.idempotency_key === ""
      || !Number.isSafeInteger(body.expected_operation_generation) || !Number.isSafeInteger(body.expected_lifecycle_generation)
      || (body.expected_receipt !== undefined && typeof body.expected_receipt !== "string")
      || Object.keys(body).some((k) => !["idempotency_key", "expected_operation_generation", "expected_lifecycle_generation", "expected_receipt"].includes(k))) return null;
    return { operation: id, principal, scope, path: value.path, body };
  }
  // A 2xx answer must name the operation that was asked for before the saved request is dropped.
  function deploymentAck(value, expect = {}) {
    const op = value?.operation;
    if (!object(op) || typeof op.operation_id !== "string" || !OPERATION_ID.test(op.operation_id)) return null;
    if (expect.operation !== undefined && op.operation_id !== expect.operation) return null;
    if (expect.kind !== undefined && op.kind !== expect.kind) return null;
    if (expect.tenant !== undefined && op.tenant_id !== expect.tenant) return null;
    return op;
  }
  // Codes where repeating the same key can never help: either nothing was recorded, or the server keeps the
  // record but a resend refuses the same way (tenant_id_conflict, operation_terminal and the cancel refusals
  // after its action record; their hints say so). The key is discarded. Any other failure may have recorded
  // the request part-way, so the same key is kept for an exact retry.
  const DEPLOY_SETTLED = new Set([
    "admin_auth_required", "read_only", "forbidden_scope", "gateway_tenant_required", "not_found", "invalid_json",
    "tenant_deployments_not_configured", "tenants_disabled", "operation_request_invalid", "operation_request_conflict",
    "tenant_id_conflict", "tenant_deployment_action_unknown", "operation_not_found", "action_request_conflict",
    "operation_generation_stale", "retirement_required", "operation_terminal", "transition_unavailable",
    "irreversible_ack_required", "pin_scope_unproven", "readers_in_progress", "retirement_in_progress", "withdrawal_capacity",
  ]);
  function deploymentRetryable(error) {
    const status = error?.status;
    if (!Number.isInteger(status) || status === 408 || status === 429 || status >= 500) return true;
    return status !== 404 && !DEPLOY_SETTLED.has(error?.code);
  }
  // Form fields of the tenant deployment forms (cleared on sign-out) and where each form reports.
  const TD = (name) => `tenant-deployment-${name}`;
  const TD_PROFILES = ["writer", "init", "reader", "scaling"];
  const TD_FIELDS = ["operation", "name", "max-vectors", "resources", "secrets", "readers-tenant", "readers-mode", "readers-min", "readers-max",
    "readers-routing", "retire-tenant", "retire-receipt", "retire-reason", ...TD_PROFILES.flatMap((p) => [`${p}-file`, `${p}-text`])].map(TD);
  const TD_BUTTONS = ["check", "cancel", "create", "readers", "retire"].map(TD);
  const TD_NOTES = TD_PROFILES.map((p) => TD(`${p}-note`));
  const TD_MESSAGES = { tdOperation: TD("operation-message"), tdCreate: TD("create-message"), tdReaders: TD("readers-message"), tdRetire: TD("retire-message") };
  // Which form each field belongs to: editing a form discards its pending request key.
  const TD_FORM_FIELDS = {
    tdCreate: ["name", "max-vectors", "resources", "secrets", "writer-file", "writer-text", "init-file", "init-text"].map(TD),
    tdReaders: ["readers-tenant", "readers-mode", "readers-min", "readers-max", "readers-routing", "reader-file", "reader-text", "scaling-file", "scaling-text"].map(TD),
    tdRetire: ["retire-tenant", "retire-receipt", "retire-reason"].map(TD),
  };
  // Profile text from the chosen file (exact bytes, UTF-8 only) or else from the paste box.
  async function deploymentProfileText(file, pasted, label) {
    if (!file) return pasted;
    if (!(file.size <= DEPLOY_CONTENT_MAX)) throw new Error(`${label}: ${file.name} is ${file.size} bytes; at most ${DEPLOY_CONTENT_MAX} (1 MiB) are allowed. No request sent`);
    const buffer = await file.arrayBuffer();
    try { return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(buffer); }
    catch { throw new Error(`${label}: ${file.name} is not UTF-8 text. Choose the text file itself; no request sent`); }
  }

  function init(ctx) {
    const $ = (id) => document.getElementById(id);
    const controls = ["tenant-list", "tenant-name", "tenant-max-vectors", "tenant-create", "tenant-id", "tenant-delete", "tenant-result",
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
      ...TD_FIELDS, ...TD_BUTTONS, ...TD_NOTES, ...Object.values(TD_MESSAGES), "tenant-deployment-readers-present", "tenant-deployment-operation-status",
      "tenant-deployment-operation-facts", "tenant-deployment-operation-help"];
    for (const name of controls) if (!$(name)) throw new Error(`admin markup is missing control #${name}`);
    const OUTPUTS = { users: "user-result", tenants: "tenant-result", tls: "tls-result", replication: "replication-result", compaction: "compaction-result", cloudBackup: "cloud-backup-result", autoFailover: "auto-failover-result", tenantDeployments: "tenant-deployment-result" };
    const TARGETS = { ...OUTPUTS, lifecycle: "tenant-result", ...TD_MESSAGES };
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
    const on = (name, area, action, describe = explain) => $(name).addEventListener("click", () => ctx.run(async () => {
      const mark = epoch;
      try { await action(mark); }
      catch (error) {
        if (!error?.stale && current(mark)) show(TARGETS[area], describe(error));
        // The panel already says it is off while agent memory is enabled; that is not a failure to report again.
        if (error?.memoryMode) { ctx.status?.(error.message); return; }
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
      if (!await ctx.confirm(`Delete tenant ${value}? The server refuses (HTTP 409 tenant_not_empty) while any collection or graph named t${value}__* exists; drop those first. API keys scoped to this tenant stop working and its user accounts are removed. The dashboard cannot undo the deletion.`)) return;
      if (!current(mark)) return;
      const result = await ctx.api("DELETE", `/v1/admin/tenants/${value}`, undefined, { data: false });
      if (current(mark)) show("tenant-result", { tenant_id: value, ...(object(result) ? result : { result }) });
    });
    // Users: the form edits one account at a time. Entering a listed ID loads its fields.
    // Graph fields appear once graph.js reports a graph build or listed accounts carry graph knobs.
    let graphAccounts = false;
    const graphInputs = ["user-graphs", "user-graph-access"].map((id) => $(id)).filter(Boolean);
    const graphForm = () => {
      const superAdmin = $("user-role").value === "super_admin";
      for (const input of graphInputs) {
        const label = input.closest?.("label");
        if (label) label.hidden = !graphAccounts;
        input.disabled = superAdmin;
        if (superAdmin) input.value = "";
      }
    };
    const graphSupport = (supported) => { graphAccounts = supported && graphUtils() !== undefined && graphInputs.length === 2; graphForm(); };
    if (typeof window !== "undefined" && typeof window.addEventListener === "function") {
      window.addEventListener("vlite:graph-status", (event) => {
        const status = event?.detail?.status;
        if (["available", "disabled", "denied"].includes(status)) graphSupport(true);
        else if (status === "absent") graphSupport(false);
      });
    }
    $("user-role").addEventListener?.("change", graphForm);
    const userFields = () => ({
      username: $("user-username").value, password: $("user-password").value, role: $("user-role").value,
      tenant: $("user-tenant").value, mustChange: $("user-must-change").checked === true, disabled: $("user-disabled").checked === true,
      ...(graphAccounts ? { graph: { graphs: $("user-graphs").value, access: $("user-graph-access").value } } : {}),
    });
    const remember = (user) => { if (object(user) && typeof user.id === "string") known.set(user.id, user); };
    const fill = (user) => {
      $("user-username").value = user.username ?? "";
      $("user-role").value = Object.hasOwn(ROLES, user.role) ? user.role : "viewer";
      $("user-tenant").value = user.tenant_id ?? "";
      $("user-must-change").checked = user.must_change_password === true;
      $("user-disabled").checked = user.disabled === true;
      $("user-password").value = "";
      if (graphAccounts) {
        const knobs = graphUtils().accountGraphFields(user);
        $("user-graphs").value = knobs.graphs;
        $("user-graph-access").value = knobs.access;
      }
      graphForm();
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
      if (value.items.some(hasGraphKnobs)) graphSupport(true);
      show("user-result", { ...view, next: "Enter a user's ID above to load and change it." });
    });
    on("user-create", "users", async (mark) => {
      need("users");
      const body = userRequest(userFields());
      const where = body.role === "super_admin" ? "the whole server" : body.tenant_id ? `tenant ${body.tenant_id}` : "every collection";
      const graphs = graphAccounts && body.role !== "super_admin" ? ` Graphs: ${graphUtils().accountGraphView(body)}.` : "";
      if (!await ctx.confirm(`Add ${ROLES[body.role].toLowerCase()} "${body.username}" with access to ${where}?${graphs}${body.must_change_password ? " They must choose a new password at their first sign-in." : ""}${body.disabled ? " The account starts disabled." : ""} Share the password with them privately.`)) return;
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
      const LABELS = { username: "username", role: "role", tenant_id: "tenant", disabled: "account disabled", must_change_password: "new password at next sign-in", graphs: "graphs", graph_access: "graph access" };
      const describe = (key, value) => key === "role" ? ROLES[value] : key === "tenant_id" ? value ?? "none (all collections)"
        : key === "graphs" ? value === null ? "role default" : value.length === 0 ? "none" : value.join(", ")
        : key === "graph_access" ? value ?? "role default" : typeof value === "boolean" ? value ? "yes" : "no" : value;
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
    // File compaction. Status polling uses ctx.background (no busy lock, no inspector);
    // actions go through ctx.run/ctx.api like every other control.
    const FIELDS = { start: "compaction-start-utc", window: "compaction-window", maxRun: "compaction-max-run",
      minFile: "compaction-min-file", minGrowth: "compaction-min-growth", growthPercent: "compaction-growth-percent",
      cooldown: "compaction-cooldown", maxSource: "compaction-max-source", history: "compaction-history-policy" };
    const SETTINGS_INPUTS = [...Object.values(FIELDS), "compaction-enabled"];
    let cstatus = null, cview = compactionView(null), dirty = false, pending = null, timer = null, failures = 0, watchedJob = null;
    const cRead = (path) => (ctx.background ?? ((p) => ctx.api("GET", p, undefined, { data: false })))(path);
    // Controls are inert while an operation runs, so move focus once it has finished.
    const focusAfterRun = (id) => setTimeout(() => { try { $(id).focus?.({ preventScroll: false }); } catch { /* best effort */ } }, 0);
    const fieldError = (key, message) => {
      const input = $(FIELDS[key] ?? key), note = $(`${FIELDS[key] ?? key}-error`);
      input.setAttribute?.("aria-invalid", message ? "true" : "false");
      if (note) { note.textContent = message || ""; note.hidden = !message; }
    };
    const cFields = () => ({ enabled: $("compaction-enabled").checked, history: $("compaction-history-policy").checked,
      ...Object.fromEntries(Object.entries(FIELDS).filter(([k]) => k !== "history").map(([k, id]) => [k, $(id).value])) });
    const cFill = (settings) => {
      const form = compactionForm(settings);
      $("compaction-enabled").checked = form.enabled;
      $("compaction-history-policy").checked = form.history;
      for (const [key, id] of Object.entries(FIELDS)) if (key !== "history") $(id).value = form[key];
      for (const key of Object.keys(FIELDS)) fieldError(key, "");
      localTime();
    };
    function localTime() {
      const match = /^([01]\d|2[0-3]):([0-5]\d)$/.exec($("compaction-start-utc").value.trim());
      if (!match) { $("compaction-local-time").textContent = "Times are in UTC (24-hour clock)."; return; }
      const at = new Date(Date.UTC(2000, 0, 1, Number(match[1]), Number(match[2])));
      $("compaction-local-time").textContent = `Times are in UTC. ${match[1]}:${match[2]} UTC is ${at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })} in this browser's time zone.`;
    }
    const cList = (id, items, render) => {
      const host = $(id);
      host.replaceChildren?.(...items.map(render));
      host.hidden = items.length === 0;
    };
    function cRender(status, announce = "") {
      cstatus = status;
      cview = compactionView(status);
      const finished = watchedJob && !cview.job && object(status?.last_job) && status.last_job.id === watchedJob ? `${compactionOutcome(status.last_job)} ` : "";
      watchedJob = cview.job?.id ?? (finished ? null : watchedJob);
      $("compaction-status").textContent = `${announce}${finished}${cview.headline}`;
      $("compaction-status").setAttribute?.("data-tone", cview.tone);
      cList("compaction-facts", cview.facts, ([term, value]) => {
        const row = document.createElement("div"), dt = document.createElement("dt"), dd = document.createElement("dd");
        dt.textContent = term; dd.textContent = value; row.append(dt, dd); return row;
      });
      cList("compaction-reasons", cview.reasons, (text) => { const li = document.createElement("li"); li.textContent = text; return li; });
      if (!dirty && object(status?.settings)) cFill(status.settings);
      gateCompaction();
      cSchedule();
    }
    function cSchedule() {
      clearTimeout(timer); timer = null;
      if (!cview.watch || !gates.compaction.allowed) return;
      const mark = epoch;
      timer = setTimeout(() => cRefresh(mark, true), document.hidden ? 30000 : 5000);
    }
    async function cRefresh(mark, background = false) {
      try {
        const status = await cRead("/v1/admin/compaction");
        if (!current(mark)) return;
        failures = 0;
        let announce = "";
        if (watchedJob && !object(status?.job) && status?.last_job?.id !== watchedJob) {
          // A later run (for example a scheduled one) replaced the watched job as the last job; read its own outcome.
          const job = await cRead(`/v1/admin/compaction/jobs/${encodeURIComponent(watchedJob)}`).then((value) => value?.job, () => null);
          if (!current(mark)) return;
          if (object(job) && job.outcome) announce = `Your earlier run: ${compactionOutcome(job)} `;
          watchedJob = null;
        }
        cRender(status, announce);
      } catch (error) {
        if (!current(mark)) return;
        if (!background) throw error;
        // Tolerate brief gaps (network errors, 503) while the server switches files; keep watching.
        if (Number.isInteger(error?.status) && error.status !== 503) { $("compaction-status").textContent = explain(error); return; }
        failures++;
        $("compaction-status").textContent = `Waiting for the server to answer (attempt ${failures}). ${cview.headline}`;
        if (failures < 60) { clearTimeout(timer); timer = setTimeout(() => cRefresh(mark, true), Math.min(30000, 5000 * failures)); }
      }
    }
    function gateCompaction(offline = false) {
      const allowed = !offline && gates.compaction.allowed;
      const settingsReady = allowed && object(cstatus?.settings) && !cview.job;
      for (const id of SETTINGS_INPUTS) $(id).disabled = !settingsReady;
      $("compaction-save").disabled = !settingsReady;
      $("compaction-refresh").disabled = !allowed;
      $("compaction-start").disabled = !allowed || !cview.canStart;
      $("compaction-cancel").disabled = !allowed || !cview.canCancel;
      $("compaction-ack-downtime").disabled = $("compaction-ack-history").disabled = !allowed || !cview.canStart;
      $("compaction-start-help").textContent = !allowed ? gates.compaction.reason
        : !cstatus ? "Load the status first."
        : cview.job ? (cview.canCancel ? "A compaction is running. You can cancel it until the server starts switching files." : "A compaction is running and is past the point where it can be cancelled.")
        : cview.canStart ? "Runs once now, outside the automatic window and size limits. Reads and writes pause until it finishes."
        : cview.reasons.length ? "Compact now is off until the problems listed under Why it cannot run are fixed."
        : "Compact now is off while the server is busy.";
    }
    $("compaction-start-utc").addEventListener("input", localTime);
    for (const id of SETTINGS_INPUTS) $(id).addEventListener("input", () => { dirty = true; });
    for (const id of ["compaction-ack-downtime", "compaction-ack-history"]) $(id).addEventListener("input", () => fieldError("compaction-ack", ""));
    on("compaction-refresh", "compaction", async (mark) => {
      need("compaction");
      dirty = false;
      await cRefresh(mark);
      if (current(mark)) show("compaction-result", "Status refreshed.");
    });
    on("compaction-start", "compaction", async (mark) => {
      need("compaction");
      if (!cstatus) throw new Error("Load the compaction status first.");
      if (!$("compaction-ack-downtime").checked || !$("compaction-ack-history").checked) {
        fieldError("compaction-ack", "Tick both statements to continue.");
        focusAfterRun("compaction-ack-downtime");
        throw new Error("Tick both acknowledgements before starting compaction.");
      }
      const budget = cstatus.settings?.max_run_seconds;
      if (!await ctx.confirm(`Compact the database file now? Reads and writes pause (HTTP 503 maintenance_in_progress) until the run finishes, up to ${budget ?? "the configured"} seconds plus the time to reopen the file. Change history from before the run is dropped: change-feed and replication readers must take a new snapshot, and graph clients may need to re-read revisions before their next conditional write. The original file and a recovery copy stay on disk. If the compacted file would not be meaningfully smaller, the original stays in use.`)) return;
      if (!current(mark)) return;
      const body = { expected_version: cstatus.settings_version, acknowledge_downtime: true, acknowledge_history: true };
      const text = JSON.stringify(body);
      // Reuse the key only to retry the identical request after an unknown outcome.
      if (pending?.body !== text) pending = { body: text, key: idempotencyKey() };
      let value;
      try {
        value = await ctx.api("POST", "/v1/admin/compaction/jobs", { ...body, idempotency_key: pending.key }, { data: false });
      } catch (error) {
        if (Number.isInteger(error?.status) && error.status < 500) pending = null;
        if (error?.code === "settings_version_conflict" && current(mark)) await cRefresh(mark).catch(() => {});
        throw error;
      }
      pending = null;
      if (!current(mark)) return;
      $("compaction-ack-downtime").checked = $("compaction-ack-history").checked = false;
      watchedJob = value?.job?.id ?? null;
      show("compaction-result", { started: value?.job?.id ?? null, replayed: value?.replayed === true, phase: value?.job?.phase ?? null });
      await cRefresh(mark);
    });
    on("compaction-cancel", "compaction", async (mark) => {
      need("compaction");
      const job = cview.job;
      if (!job || !cview.canCancel) throw new Error("No compaction can be cancelled right now.");
      if (!await ctx.confirm("Cancel the running compaction? It stops at the next safe point before the server switches files, and the original file stays in use. Reads and writes resume once the original file is reopened.")) return;
      if (!current(mark)) return;
      const value = await ctx.api("POST", `/v1/admin/compaction/jobs/${encodeURIComponent(job.id)}/cancel`, undefined, { data: false });
      if (!current(mark)) return;
      show("compaction-result", { cancel_requested: value?.job?.cancel_requested === true, phase: value?.job?.phase ?? null });
      await cRefresh(mark);
    });
    on("compaction-save", "compaction", async (mark) => {
      need("compaction");
      if (!cstatus?.settings) throw new Error("Load the compaction status first.");
      const { settings, errors } = compactionSettings(cFields(), cstatus.test_profile === true ? 0 : 64 * MIB);
      for (const key of Object.keys(FIELDS)) fieldError(key, errors[key] ?? "");
      if (!settings) {
        focusAfterRun(FIELDS[Object.keys(errors)[0]]);
        throw new Error("Fix the highlighted settings; nothing was saved.");
      }
      const summary = settings.enabled
        ? `Turn on automatic compaction daily at ${settings.daily_start_utc} UTC for up to ${settings.window_minutes} minutes? A run starts only when the file is at least ${formatBytes(settings.min_file_bytes)}, has grown by at least ${formatBytes(settings.min_growth_bytes)} and ${settings.min_growth_percent}% since the last compaction, and at most once every ${settings.cooldown_hours} hours. Each run pauses reads and writes and drops change history from before it; files above ${formatBytes(settings.max_source_bytes)} are skipped.`
        : "Save these compaction settings with automatic compaction off? Nothing runs automatically; Compact now still works.";
      if (!await ctx.confirm(summary)) return;
      if (!current(mark)) return;
      let value;
      try {
        value = await ctx.api("PUT", "/v1/admin/compaction/config", { expected_version: cstatus.settings_version, settings, acknowledge_history: settings.history_policy === "resnapshot_allowed" }, { data: false });
      } catch (error) {
        if (error?.code === "settings_version_conflict" && current(mark)) await cRefresh(mark).catch(() => {});
        throw error;
      }
      if (!current(mark)) return;
      dirty = false;
      cRender(value);
      show("compaction-result", `Saved. Automatic compaction is ${settings.enabled ? "on" : "off"} (settings version ${value?.settings_version ?? "?"}).`);
    });
    // Cloud backup to Azure Blob. Status loads on Refresh status; nothing is fetched automatically.
    const CLOUD_INPUTS = [...Object.values(CLOUD_FIELDS), "cloud-backup-enabled", "cloud-backup-retention"];
    let cbState = null, cbView = cloudBackupView(null), cbDirty = false, cbSets = null;
    const cloudFieldError = (id, message) => {
      const input = $(id), note = $(`${id}-error`);
      input.setAttribute?.("aria-invalid", message ? "true" : "false");
      if (note) { note.textContent = message || ""; note.hidden = !message; }
    };
    const cloudFields = () => ({ enabled: $("cloud-backup-enabled").checked, retention: $("cloud-backup-retention").checked,
      ...Object.fromEntries(Object.entries(CLOUD_FIELDS).map(([key, id]) => [key, $(id).value])) });
    const cloudFill = (policy) => {
      const form = cloudBackupForm(policy);
      $("cloud-backup-enabled").checked = form.enabled;
      $("cloud-backup-retention").checked = form.retention;
      for (const [key, id] of Object.entries(CLOUD_FIELDS)) { $(id).value = form[key]; cloudFieldError(id, ""); }
    };
    const factRow = ([term, value]) => {
      const row = document.createElement("div"), dt = document.createElement("dt"), dd = document.createElement("dd");
      dt.textContent = term; dd.textContent = value; row.append(dt, dd); return row;
    };
    function cloudRender(state) {
      cbState = object(state) ? state : null;
      cbView = cloudBackupView(cbState);
      $("cloud-backup-status").textContent = cbView.headline;
      $("cloud-backup-status").setAttribute?.("data-tone", cbView.tone);
      cList("cloud-backup-facts", cbView.facts, factRow);
      cList("cloud-backup-reasons", cbView.reasons, (text) => { const li = document.createElement("li"); li.textContent = text; return li; });
      const target = object(cbState?.policy?.target) ? cbState.policy.target : null;
      const note = $("cloud-backup-endpoint-note");
      note.textContent = !target ? ""
        : target.auth && target.auth !== "managed_identity" ? "This server signs in to a local test emulator with a key set through the REST API. This page shows its status but cannot change its settings."
        : target.endpoint ? `Storage address: ${target.endpoint}.` : "";
      note.hidden = !note.textContent;
      if (!cbDirty) cloudFill(object(cbState?.policy) ? cbState.policy : null);
      gateCloudBackup();
    }
    const selectedSet = () => cbSets?.items.find((item) => item.id === $("cloud-backup-set-id").value) ?? null;
    const option = (value, text) => { const o = document.createElement("option"); o.value = value; o.textContent = text; return o; };
    function cloudSelection() {
      const item = selectedSet();
      cList("cloud-backup-set-facts", cloudBackupSetFacts(item), factRow);
      const pins = $("cloud-backup-pin-id"), previous = pins.value;
      pins.replaceChildren?.(...(item?.pins ?? []).map((pin) => option(pin.id, pin.label)));
      pins.value = item?.pins.some((pin) => pin.id === previous) ? previous : item?.pins[0]?.id ?? "";
      gateCloudBackup();
    }
    function cloudFillSets(value) {
      cbSets = cloudBackupSets(value);
      const select = $("cloud-backup-set-id"), previous = select.value;
      select.replaceChildren?.(...cbSets.items.map((item) => option(item.id, item.label)));
      select.value = cbSets.items.some((item) => item.id === previous) ? previous : cbSets.items[0]?.id ?? "";
      cloudSelection();
      const count = cbSets.items.length;
      return `${count ? `${count} stored backup${count === 1 ? "" : "s"} found, newest first.` : "No stored backups yet."}${cbSets.removing ? ` ${cbSets.removing} more ${cbSets.removing === 1 ? "is" : "are"} being removed.` : ""}`;
    }
    function gateCloudBackup(offline = false) {
      const allowed = !offline && gates.cloudBackup.allowed;
      const editable = allowed && !cbView.running && !(cbState?.policy?.target?.auth && cbState.policy.target.auth !== "managed_identity");
      for (const id of CLOUD_INPUTS) $(id).disabled = !editable;
      $("cloud-backup-save").disabled = !editable;
      $("cloud-backup-refresh").disabled = $("cloud-backup-sets").disabled = !allowed;
      $("cloud-backup-run").disabled = $("cloud-backup-collect").disabled = !allowed || !cbView.canRun;
      $("cloud-backup-help").textContent = !allowed ? gates.cloudBackup.reason
        : !cbState ? "Select Refresh status first."
        : cbView.uncertain ? "Back up now and Clean up now are off until the saved settings are confirmed."
        : !cbView.configured ? "Save the storage details below before backing up."
        : cbView.running ? "A backup or clean-up is running. Select Refresh status to see when it finishes."
        : "Back up now uploads a new backup at once and may take several minutes. Clean up now removes old and unfinished backups as the settings allow.";
      const item = allowed ? selectedSet() : null;
      const pin = $("cloud-backup-pin-id").value;
      $("cloud-backup-set-id").disabled = !allowed || !cbSets?.items.length;
      $("cloud-backup-drill").disabled = $("cloud-backup-restore").disabled = !item?.complete || !cbView.canRun;
      $("cloud-backup-pin-reason").disabled = $("cloud-backup-pin").disabled = !item?.complete;
      $("cloud-backup-pin-id").disabled = $("cloud-backup-force").disabled = $("cloud-backup-unpin").disabled = !item || !pin;
      $("cloud-backup-sets-help").textContent = !allowed ? gates.cloudBackup.reason
        : !cbSets ? "Select Load stored backups to list the backups in your storage account."
        : !cbSets.items.length ? "There are no stored backups yet."
        : !item ? "Choose a backup."
        : !item.complete ? "This backup is not complete, so it cannot be tested, restored or held. Clean up now removes unfinished uploads."
        : !cbView.canRun ? "Test and Restore are off while a backup runs or the settings are not confirmed; select Refresh status."
        : "Test this backup checks that it can be read and restored without changing anything. Restore as a new copy writes a separate database file; the live database is not changed.";
    }
    for (const id of CLOUD_INPUTS) $(id).addEventListener("input", () => { cbDirty = true; if (Object.values(CLOUD_FIELDS).includes(id)) cloudFieldError(id, ""); });
    $("cloud-backup-pin-reason").addEventListener("input", () => cloudFieldError("cloud-backup-pin-reason", ""));
    $("cloud-backup-set-id").addEventListener("change", cloudSelection);
    $("cloud-backup-pin-id").addEventListener("change", () => gateCloudBackup());
    on("cloud-backup-refresh", "cloudBackup", async (mark) => {
      need("cloudBackup");
      cbDirty = false;
      const value = await ctx.api("GET", "/v1/admin/cloud-backup", undefined, { data: false });
      if (!current(mark)) return;
      cloudRender(value);
      show("cloud-backup-result", "Status refreshed.");
    });
    on("cloud-backup-save", "cloudBackup", async (mark) => {
      need("cloudBackup");
      if (!cbState) throw new Error("Select Refresh status first.");
      const { policy, errors } = cloudBackupPolicy(cloudFields(), cbState.policy);
      for (const [key, id] of Object.entries(CLOUD_FIELDS)) cloudFieldError(id, errors[key] ?? "");
      if (!policy) {
        focusAfterRun(CLOUD_FIELDS[Object.keys(errors)[0]]);
        throw new Error("Fix the highlighted settings; nothing was saved.");
      }
      const where = `account ${policy.target.account}, container ${policy.target.container}, folder ${policy.target.prefix}`;
      const summary = `Save cloud backup settings (${where})? ${policy.enabled ? `Automatic backups are on: one every ${cloudInterval(policy.interval_secs)}.` : "Automatic backups are off; nothing is uploaded until you select Back up now."} ${policy.retention_enabled ? `Old backups are deleted automatically, keeping the newest ${policy.keep_sets} and any younger than ${policy.keep_days} days.` : "Old backups are never deleted automatically."} Saving does not upload or delete anything by itself.`;
      if (!await ctx.confirm(summary)) return;
      if (!current(mark)) return;
      const value = await ctx.api("PUT", "/v1/admin/cloud-backup", policy, { data: false });
      if (!current(mark)) return;
      cbDirty = false;
      cloudRender(value);
      show("cloud-backup-result", `Saved. Automatic backups are ${policy.enabled ? "on" : "off"}.`);
    });
    on("cloud-backup-run", "cloudBackup", async (mark) => {
      need("cloudBackup");
      if (!cbView.canRun) throw new Error("Select Refresh status and make sure cloud backup is set up first.");
      if (!await ctx.confirm(`Back up to account ${cbState.policy.target.account} now? This uploads a new full backup and may take several minutes. Each backup is stored separately; old ones are removed only if automatic deletion is on.`)) return;
      if (!current(mark)) return;
      const value = await ctx.api("POST", "/v1/admin/cloud-backup/run", undefined, { data: false });
      if (!current(mark)) return;
      show("cloud-backup-result", cloudRunOutcome(value));
      cloudRender(await ctx.api("GET", "/v1/admin/cloud-backup", undefined, { data: false }));
    });
    on("cloud-backup-collect", "cloudBackup", async (mark) => {
      need("cloudBackup");
      if (!cbView.canRun) throw new Error("Select Refresh status and make sure cloud backup is set up first.");
      const retention = cbState.policy.retention_enabled === true;
      if (!await ctx.confirm(`Clean up now? This removes unfinished uploads older than ${cbState.policy.orphan_grace_hours} hours${retention ? ` and old backups beyond the newest ${cbState.policy.keep_sets} that are older than ${cbState.policy.keep_days} days` : ". Old complete backups are kept because automatic deletion is off"}. Held backups and the newest verified backup are never removed. Removed backups cannot be recovered.`)) return;
      if (!current(mark)) return;
      const value = await ctx.api("POST", "/v1/admin/cloud-backup/collect", undefined, { data: false });
      if (!current(mark)) return;
      show("cloud-backup-result", `Clean-up ${cloudCollectionText(value)}`);
      cloudRender(await ctx.api("GET", "/v1/admin/cloud-backup", undefined, { data: false }));
    });
    const reloadSets = async (mark) => {
      const value = await ctx.api("GET", "/v1/admin/cloud-backup/sets", undefined, { data: false });
      return current(mark) ? cloudFillSets(value) : "";
    };
    on("cloud-backup-sets", "cloudBackup", async (mark) => {
      need("cloudBackup");
      const value = await ctx.api("GET", "/v1/admin/cloud-backup/sets", undefined, { data: false });
      if (current(mark)) show("cloud-backup-result", cloudFillSets(value));
    });
    on("cloud-backup-drill", "cloudBackup", async (mark) => {
      need("cloudBackup");
      const item = selectedSet();
      if (!item?.complete) throw new Error("Choose a complete backup first.");
      if (!await ctx.confirm(`Test backup ${item.id}? The server downloads it, checks every file and opens a temporary copy, then deletes the copy. The live database and the stored backup are not changed. This may take several minutes.`)) return;
      if (!current(mark)) return;
      const value = await ctx.api("POST", `/v1/admin/cloud-backup/sets/${encodeURIComponent(item.id)}/drill`, undefined, { data: false });
      if (!current(mark)) return;
      const text = `Restore test of ${item.id}: ${cloudDrillText(value?.drill ?? value)}.`;
      show("cloud-backup-result", `${text} ${await reloadSets(mark)}`);
    });
    on("cloud-backup-restore", "cloudBackup", async (mark) => {
      need("cloudBackup");
      const item = selectedSet();
      if (!item?.complete) throw new Error("Choose a complete backup first.");
      if (!await ctx.confirm(`Restore backup ${item.id} as a new copy? The server downloads it and writes a separate database file in its admin directory. The live database is not changed; to switch to the copy, select it under Backups and restore and restart the server. Credentials are not included in backups and must be issued again.`)) return;
      if (!current(mark)) return;
      const value = await ctx.api("POST", `/v1/admin/cloud-backup/sets/${encodeURIComponent(item.id)}/restore`, undefined, { data: false });
      if (current(mark)) show("cloud-backup-result", cloudRestoreOutcome(value));
    });
    on("cloud-backup-pin", "cloudBackup", async (mark) => {
      need("cloudBackup");
      const item = selectedSet();
      if (!item?.complete) throw new Error("Choose a complete backup first.");
      const reason = $("cloud-backup-pin-reason").value.trim();
      const problem = !reason ? "Enter a reason." : utf8Length(reason) > 200 ? "Use at most 200 characters (accented letters and symbols count as two or more)."
        : /[\u0000-\u001f\u007f-\u009f]/.test(reason) ? "Remove line breaks and other control characters." : "";
      cloudFieldError("cloud-backup-pin-reason", problem);
      if (problem) { focusAfterRun("cloud-backup-pin-reason"); throw new Error(`${problem} Nothing was held.`); }
      const value = await ctx.api("POST", `/v1/admin/cloud-backup/sets/${encodeURIComponent(item.id)}/pins`, { reason }, { data: false });
      if (!current(mark)) return;
      $("cloud-backup-pin-reason").value = "";
      show("cloud-backup-result", `Backup ${item.id} is held (hold ${value?.pin?.id ?? "created"}); clean-up will not remove it until the hold is released. ${await reloadSets(mark)}`);
    });
    on("cloud-backup-unpin", "cloudBackup", async (mark) => {
      need("cloudBackup");
      const item = selectedSet(), pin = $("cloud-backup-pin-id").value, force = $("cloud-backup-force").checked;
      if (!item || !pin) throw new Error("Choose a backup and one of its holds first.");
      if (!await ctx.confirm(`Release hold ${pin} on backup ${item.id}? Clean-up may then remove the backup if it is old enough and automatic deletion is on.${force ? " You confirmed the operation that made this hold is no longer running." : ""}`)) return;
      if (!current(mark)) return;
      const path = `/v1/admin/cloud-backup/sets/${encodeURIComponent(item.id)}/pins/${encodeURIComponent(pin)}${force ? "?force=true" : ""}`;
      await ctx.api("DELETE", path, undefined, { data: false });
      if (!current(mark)) return;
      $("cloud-backup-force").checked = false;
      show("cloud-backup-result", `Hold ${pin} released. ${await reloadSets(mark)}`);
    });
    // Automatic failover: one shared on/off switch read by every supervisor. Status loads on Refresh status.
    let afState = null, afView = autoFailoverView(null);
    function afRender(state) {
      afState = object(state) ? state : null;
      afView = autoFailoverView(afState);
      $("auto-failover-status").textContent = afView.headline;
      $("auto-failover-status").setAttribute?.("data-tone", afView.tone);
      cList("auto-failover-facts", afView.facts, factRow);
      gateAutoFailover();
    }
    function gateAutoFailover(offline = false) {
      const allowed = !offline && gates.autoFailover.allowed;
      const changeable = gates.autoFailover.change !== false;
      const toggle = $("auto-failover-toggle");
      $("auto-failover-refresh").disabled = !allowed;
      toggle.disabled = !allowed || !changeable || !afState || !afView.canToggle;
      const turnOff = afView.enabled || afView.invalid;
      toggle.textContent = turnOff ? "Turn off automatic failover" : "Turn on automatic failover";
      toggle.classList?.toggle("danger", !turnOff);
      $("auto-failover-help").textContent = !allowed ? gates.autoFailover.reason
        : !changeable ? `${gates.autoFailover.changeReason} Refresh status still shows the current setting.`
        : !afState ? "Select Refresh status first."
        : !afView.available ? "This server cannot switch automatic failover. Manual failover still works."
        : turnOff ? "Turning it off writes a fresh disabled revision. A promotion that has not passed its final transfer check will stop; one already past that check may complete."
        : "Turning it on lets supervisors fail over without review. It does not start a supervisor.";
    }
    on("auto-failover-refresh", "autoFailover", async (mark) => {
      need("autoFailover");
      const value = await ctx.api("GET", "/v1/admin/auto-failover", undefined, { data: false });
      if (!current(mark)) return;
      afRender(value);
      show("auto-failover-result", "Status refreshed.");
    });
    on("auto-failover-toggle", "autoFailover", async (mark) => {
      need("autoFailover");
      // Without an admin token the server would answer 401, which signs the dashboard out.
      if (gates.autoFailover.change === false) throw new Error(gates.autoFailover.changeReason);
      if (!afState || !afView.canToggle) throw new Error("Select Refresh status first.");
      const enable = !(afView.enabled || afView.invalid);
      const prompt = enable
        ? "Turn on automatic failover? It is NOT yet proven safe (GATE-005 has not passed). A supervisor that loses contact with the writer will fence it and promote its standby without asking anyone, and changes the writer had not yet copied can be lost. Turn it off at any time to stop it."
        : "Turn off automatic failover? This writes a fresh disabled revision. A promotion that has not passed its final transfer check will stop; one already past that check may complete. Manual failover still works.";
      if (!await ctx.confirm(prompt)) return;
      if (!current(mark)) return;
      const body = autoFailoverRequest(afState, enable);
      let value;
      try { value = await ctx.api("PUT", "/v1/admin/auto-failover", body, { data: false }); }
      catch (error) {
        // The stored setting is now unknown, so the next switch needs a fresh status.
        if (current(mark)) afRender(null);
        throw error;
      }
      if (!current(mark)) return;
      afRender(value);
      show("auto-failover-result", enable ? "Automatic failover is on (not yet proven safe)." : "Automatic failover is off.");
    });
    // Tenant deployments: the list is paged by operation ID and nothing loads until Load operations.
    // The forms below it only record requests (create, readers, retire) or a cancel; each asks for confirmation first.
    let tdAfter = null, tdPage = 0;
    // Request keys kept after an unknown outcome, so the same button repeats the same request: form -> { text, key }.
    let tdPending = {};
    // The cancel kept after an unknown outcome: { operation, principal, scope, path, body } (the body includes its key).
    let tdCancelPending = null;
    // The account the server verified for this sign-in (deploymentPrincipal), and the last one, so that a session found
    // to have ended after the page already let go of it still deletes that account's saved cancels.
    let tdPrincipal = null, tdLastPrincipal = null;
    // The API address this page's requests go to (deploymentApiScope); saved cancels are filed and found under it only.
    const tdScope = deploymentApiScope(ctx.apiBase, globalThis.document?.baseURI ?? globalThis.location?.href);
    // Each sent cancel is also kept in this tab's sessionStorage under its API address and account (it holds only the
    // operation ID, the key and the quoted values), so a cancel whose release step failed can still be resent exactly
    // after a reload by the same account through the same API address. Without a verified account or an API address
    // nothing is read or written: the cancel stays in memory only.
    const tdCancelKey = (principal, id) => `${TD_CANCEL_STORE}${encodeURIComponent(tdScope)}/${principal}.${id}`;
    const tdCancelSaved = (id) => {
      if (!tdPrincipal || !tdScope) return null;
      try { return deploymentSavedCancel(JSON.parse(globalThis.sessionStorage?.getItem(tdCancelKey(tdPrincipal, id)) ?? "null"), id, tdPrincipal, tdScope); }
      catch { return null; }
    };
    const tdCancelForget = (id) => {
      if (!tdPrincipal || !tdScope) return;
      try { globalThis.sessionStorage?.removeItem(tdCancelKey(tdPrincipal, id)); } catch { /* storage unavailable */ }
    };
    function tdCancelKeep(pending) {
      tdCancelPending = pending;
      if (!pending.principal || !pending.scope || pending.scope !== tdScope) return;
      try { globalThis.sessionStorage?.setItem(tdCancelKey(pending.principal, pending.operation), JSON.stringify(pending)); } catch { /* storage unavailable */ }
    }
    function tdCancelDrop(id) {
      if (tdCancelPending?.operation === id) tdCancelPending = null;
      tdCancelForget(id);
    }
    const tdCancelSavedFor = (id) => (tdCancelPending?.operation === id ? tdCancelPending : tdCancelSaved(id));
    const tdResendHelp = (saved, sep) => `${RELEASE_PENDING} Select Cancel to resend your earlier cancel exactly; that finishes the release.${sep}${deploymentCancelScope(saved.principal, saved.scope)}`;
    // Deletes this API address's saved cancels whose owner ends(owner), and every cancel key whose owner cannot be read
    // (an older page's or a damaged key). Another API address's saved cancels are left alone.
    function tdCancelSweep(ends) {
      try {
        const storage = globalThis.sessionStorage;
        if (!storage) return;
        const keys = [];
        for (let i = 0; i < storage.length; i++) keys.push(storage.key(i));
        for (const key of keys) {
          const owner = deploymentCancelOwner(key, tdScope);
          if (owner === null || (typeof owner === "string" && ends(owner))) storage.removeItem(key);
        }
      } catch { /* storage unavailable */ }
    }
    // The status last shown in the operation box.
    let tdOp = null;
    function tdStatus(text, tone) {
      $("tenant-deployment-status").textContent = text;
      $("tenant-deployment-status").setAttribute?.("data-tone", tone);
    }
    const tdOperationField = () => $(TD("operation")).value.trim().toLowerCase();
    function tdUse(view) {
      if (!gates.tenantDeployments.allowed) return;
      if (view.operation) {
        if (tdCancelPending?.operation !== view.operation) tdCancelPending = null;
        $(TD("operation")).value = view.operation;
      }
      if (view.tenant) {
        for (const id of [TD("readers-tenant"), TD("retire-tenant")]) $(id).value = view.tenant;
        delete tdPending.tdReaders; delete tdPending.tdRetire;
      }
      // A receipt left from another operation would pair this tenant with the wrong receipt.
      if (view.receipt) $(TD("retire-receipt")).value = view.receipt;
      else if (view.tenant) $(TD("retire-receipt")).value = "";
      show(TD_MESSAGES.tdOperation, `Filled in operation ${view.operation ?? "--"}${view.tenant ? ` and its tenant ID ${view.tenant} in the Readers and Retire forms` : ""}${view.receipt ? ", with its receipt" : view.tenant ? " (it has no receipt, so the Retire receipt box was emptied)" : ""}. Select Check status or Cancel.`);
      gateTenantDeployments();
    }
    const tdItem = (view) => {
      const item = document.createElement("li"), title = document.createElement("strong"), facts = document.createElement("dl");
      title.textContent = view.headline;
      facts.className = "compaction-facts";
      facts.append(...view.facts.map(factRow));
      item.append(title, facts);
      if (view.operation) {
        const use = document.createElement("button");
        use.type = "button";
        use.className = "secondary";
        use.textContent = "Use this operation";
        use.setAttribute?.("aria-label", `Use operation ${view.operation}`);
        use.addEventListener("click", () => tdUse(view));
        item.append(use);
      }
      return item;
    };
    function tdShowOperation(op) {
      const view = deploymentView(op);
      tdOp = op;
      $(TD("operation")).value = view.operation ?? "";
      $(TD("operation-status")).textContent = view.headline;
      cList(TD("operation-facts"), view.facts, factRow);
      gateTenantDeployments();
      return view;
    }
    function gateTenantDeployments(offline = false) {
      const allowed = !offline && gates.tenantDeployments.allowed;
      $("tenant-deployment-refresh").disabled = !allowed;
      $("tenant-deployment-next").disabled = !allowed || tdAfter === null;
      $("tenant-deployment-help").textContent = !allowed ? gates.tenantDeployments.reason
        : tdAfter !== null ? "Next page shows the operations after the last one listed."
        : `Load operations shows the first ${DEPLOY_PAGE}. Nothing loads automatically.`;
      for (const id of [...TD_FIELDS, ...TD_BUTTONS]) $(id).disabled = !allowed;
      const shown = tdOp !== null && tdOperationField() === tdOp.operation_id;
      const releasing = shown && deploymentReleasePending(tdOp);
      const resend = releasing && tdCancelSavedFor(tdOp.operation_id) !== null;
      const ended = shown && !releasing && deploymentTerminal(tdOp);
      $(TD("cancel")).disabled = !allowed || ended;
      $(TD("operation-help")).textContent = !allowed ? gates.tenantDeployments.reason
        : releasing ? (resend ? tdResendHelp(tdCancelSavedFor(tdOp.operation_id), " ") : RELEASE_LOST)
        : ended ? "This operation has ended, so it cannot be cancelled. Enter another operation ID to cancel a different one."
        : "Check status reads the operation's current state. Cancel reads it again, shows what will be cancelled and asks you to confirm.";
    }
    function tdClear() {
      tdAfter = null; tdPage = 0;
      cList("tenant-deployment-list", [], () => null);
    }
    async function tdLoad(mark, after) {
      need("tenantDeployments");
      let value;
      try { value = await ctx.api("GET", deploymentListPath(after), undefined, { data: false }); }
      catch (error) {
        if (!current(mark)) throw error;
        const blocked = deploymentBlocked(error);
        // A refusal or a rejected position starts over; a network or server failure keeps the page so the same button can try again.
        const retryable = !blocked && (error?.status === undefined || error.status >= 500 || error.status === 429 || error.status === 408);
        if (!retryable) tdClear();
        tdStatus(blocked || (tdPage > 0
          ? `Could not load tenant deployment operations; page ${tdPage} below is from before. Try again.`
          : "Could not load tenant deployment operations. See the message below."), "blocked");
        gateTenantDeployments();
        if (!blocked) throw error;
        show("tenant-deployment-result", deploymentExplain(error));
        ctx.status?.(blocked);
        return;
      }
      if (!current(mark)) return;
      let page;
      try { page = deploymentList(value, after); }
      catch (error) {
        tdClear();
        tdStatus("Could not load tenant deployment operations. See the message below.", "blocked");
        gateTenantDeployments();
        throw error;
      }
      tdPage = after === null ? 1 : tdPage + 1;
      tdAfter = page.next;
      cList("tenant-deployment-list", page.items, tdItem);
      const count = page.items.length;
      tdStatus(count === 0 && tdPage === 1 ? "No tenant deployment operations are recorded yet."
        : `Page ${tdPage}: ${count} operation${count === 1 ? "" : "s"}${tdAfter !== null ? "; more on the next page" : ""}. Listed in operation ID order, not by time.`, "ready");
      gateTenantDeployments();
      show("tenant-deployment-result", "");
    }
    // After a change: reload the first page; a failure there is reported below the list, not as a failed change.
    async function tdReload(mark) {
      try { await tdLoad(mark, null); }
      catch (error) { if (current(mark)) show("tenant-deployment-result", deploymentExplain(error)); }
    }
    on("tenant-deployment-refresh", "tenantDeployments", (mark) => tdLoad(mark, null), deploymentExplain);
    on("tenant-deployment-next", "tenantDeployments", async (mark) => {
      if (tdAfter === null) throw new Error("Select Load operations first.");
      await tdLoad(mark, tdAfter);
    }, deploymentExplain);
    const tdProfile = (name, label) => deploymentProfileText($(TD(`${name}-file`)).files?.[0], $(TD(`${name}-text`)).value, label);
    function tdClearForm(form) {
      for (const id of TD_FORM_FIELDS[form]) {
        if (id === TD("readers-mode")) $(id).value = "present";
        else if (id === TD("readers-routing")) $(id).value = "writer_only";
        else $(id).value = "";
      }
      if (form === "tdReaders") $(TD("readers-present")).hidden = false;
      for (const p of form === "tdCreate" ? ["writer", "init"] : form === "tdReaders" ? ["reader", "scaling"] : []) $(TD(`${p}-note`)).textContent = "";
      delete tdPending[form];
    }
    // Records one request. The key is reused only while the exact same request is retried after an unknown outcome.
    async function tdSubmit(mark, form, build) {
      need("tenantDeployments");
      const submission = await build();
      if (!current(mark)) return;
      const text = JSON.stringify([submission.path, submission.request, submission.contents]);
      const retrying = tdPending[form]?.text === text;
      const key = retrying ? tdPending[form].key : idempotencyKey();
      const note = retrying ? "\nThis repeats your earlier request whose outcome is unknown, with the same request key, so it is recorded at most once." : "";
      if (!await ctx.confirm(`${submission.confirm}${note}`)) { if (current(mark)) show(TD_MESSAGES[form], "Nothing was sent."); return; }
      if (!current(mark)) return;
      const entry = { text, key };
      tdPending[form] = entry;
      show(TD_MESSAGES[form], "Sending…");
      const { request, contents } = submission;
      const inspectorSummary = {
        documents: contents.length, resources: request.resources?.length ?? 0, secret_versions: request.secret_versions?.length ?? 0,
        profile_digests: [request.writer_profile, request.initialization, request.fleet?.reader_profile, request.fleet?.scaling_rule].filter(Boolean),
      };
      let value;
      try { value = await ctx.api("POST", submission.path, { idempotency_key: key, request, contents }, { data: false, inspectorSummary }); }
      catch (error) {
        if (current(mark) && tdPending[form] === entry) {
          if (deploymentRetryable(error)) error.deployRetry = true;
          else delete tdPending[form];
        }
        throw error;
      }
      if (!current(mark)) return;
      // An answer that does not name this request is an unknown outcome: the key stays for an exact resend.
      const ack = deploymentAck(value, { kind: submission.request.kind, tenant: submission.request.tenant_id });
      if (!ack) {
        const error = new Error("The server's answer did not confirm this request (it did not name a matching operation).");
        if (tdPending[form] === entry) error.deployRetry = true;
        throw error;
      }
      if (tdPending[form] === entry) tdClearForm(form);
      const view = tdShowOperation(ack);
      show(TD_MESSAGES[form], [
        value.duplicate === true ? "This was already recorded earlier with the same request key; nothing new was added." : "Recorded. Nothing was deployed (cloud execution is switched off, OD-002).",
        view.headline,
        `Next step: ${typeof ack.next_step === "string" ? ack.next_step : "--"}`,
        `Operation ID: ${view.operation ?? "--"} (now in the operation box below, where Cancel withdraws it)`,
        `Tenant ID: ${view.tenant ?? "--"}`,
      ].join("\n"));
      await tdReload(mark);
    }
    on(TD("create"), "tdCreate", (mark) => tdSubmit(mark, "tdCreate", async () => deploymentCreateSubmission({
      name: $(TD("name")).value, maxVectors: $(TD("max-vectors")).value,
      writer: await tdProfile("writer", "Writer profile"), initialization: await tdProfile("init", "Initialization settings"),
      resources: $(TD("resources")).value, secrets: $(TD("secrets")).value,
    })), deploymentExplain);
    on(TD("readers"), "tdReaders", (mark) => tdSubmit(mark, "tdReaders", async () => {
      const present = $(TD("readers-mode")).value === "present";
      return deploymentReadersSubmission({
        tenant: $(TD("readers-tenant")).value, mode: $(TD("readers-mode")).value,
        min: $(TD("readers-min")).value, max: $(TD("readers-max")).value, routing: $(TD("readers-routing")).value,
        reader: present ? await tdProfile("reader", "Reader profile") : "", scaling: present ? await tdProfile("scaling", "Scaling rule") : "",
      });
    }), deploymentExplain);
    on(TD("retire"), "tdRetire", (mark) => tdSubmit(mark, "tdRetire", async () => deploymentRetireSubmission({
      tenant: $(TD("retire-tenant")).value, receipt: $(TD("retire-receipt")).value, reason: $(TD("retire-reason")).value,
    })), deploymentExplain);
    function tdOperationId() {
      const id = tdOperationField();
      if (!OPERATION_ID.test(id)) throw new Error("Operation ID must be the 32-character ID from the list (Use this operation fills it in) or from a result above; nothing was sent");
      return id;
    }
    // Always reads the operation first: Cancel must quote its current tokens.
    async function tdRead(mark, id) {
      const value = await ctx.api("GET", `/v1/admin/tenant-deployment-operations/${id}`, undefined, { data: false });
      if (!current(mark)) return null;
      if (!object(value?.operation) || value.operation.operation_id !== id) throw new Error("The server sent an unexpected operation status. Select Check status to try again.");
      tdShowOperation(value.operation);
      return value.operation;
    }
    on(TD("check"), "tdOperation", async (mark) => {
      need("tenantDeployments");
      const id = tdOperationId();
      const op = await tdRead(mark, id);
      if (!op) return;
      if (deploymentReleasePending(op)) {
        const saved = tdCancelSavedFor(id);
        show(TD_MESSAGES.tdOperation, saved ? tdResendHelp(saved, "\n") : RELEASE_LOST);
        return;
      }
      show(TD_MESSAGES.tdOperation, deploymentTerminal(op) ? "This operation has ended; it cannot be cancelled." : `Next step: ${typeof op.next_step === "string" ? op.next_step : "--"}`);
    }, deploymentExplain);
    on(TD("cancel"), "tdOperation", async (mark) => {
      need("tenantDeployments");
      const id = tdOperationId();
      show(TD_MESSAGES.tdOperation, "Reading the operation's current status first…");
      const op = await tdRead(mark, id);
      if (!op) return;
      const saved = tdCancelSavedFor(id);
      const releasing = deploymentReleasePending(op);
      if (!releasing && deploymentTerminal(op)) {
        tdCancelDrop(id);
        show(TD_MESSAGES.tdOperation, "This operation has already ended, so there is nothing to cancel. Nothing was sent.");
        return;
      }
      // With no saved cancel for this account, a pending release gets a new cancel quoting the status just read.
      const pending = saved ?? { operation: id, principal: tdPrincipal, scope: tdScope, ...deploymentCancelRequest(op, idempotencyKey()) };
      if (!await ctx.confirm(releasing ? deploymentReleaseConfirm(op, saved !== null) : deploymentCancelConfirm(op, saved !== null))) { if (current(mark)) show(TD_MESSAGES.tdOperation, "Nothing was sent."); return; }
      if (!current(mark)) return;
      tdCancelKeep(pending);
      show(TD_MESSAGES.tdOperation, "Sending the cancel…");
      let value;
      try { value = await ctx.api("POST", pending.path, pending.body, { data: false }); }
      catch (error) {
        if (current(mark) && tdCancelPending === pending) {
          if (deploymentRetryable(error)) error.deployRetry = true;
          else tdCancelDrop(id);
        }
        if (releasing && saved === null && error?.code === "operation_terminal") error.deployHint = RELEASE_REFUSED;
        if (error?.code === "operation_generation_stale" && current(mark)) focusAfterRun(TD("check"));
        throw error;
      }
      if (!current(mark)) return;
      // Keep the saved cancel until the answer names this operation and its reservation is released.
      const ack = deploymentAck(value, { operation: id });
      if (!ack) {
        const error = new Error("The server's answer did not confirm the cancel (it did not name this operation).");
        if (tdCancelPending === pending) error.deployRetry = true;
        throw error;
      }
      const stillReserved = deploymentReleasePending(ack);
      if (!stillReserved) tdCancelDrop(id);
      const view = tdShowOperation(ack);
      show(TD_MESSAGES.tdOperation, [
        stillReserved ? `${RELEASE_PENDING} Select Cancel again to resend the same cancel and finish the release.`
          : ack.phase === "cancelling" ? "Cancellation started but is not finished. Select Cancel again to finish it."
          : ack.phase === "cancelled" ? "Cancelled. Cloud execution is switched off (OD-002), so nothing in Azure was touched." : "The server answered:",
        view.headline,
        `Next step: ${typeof ack.next_step === "string" ? ack.next_step : "--"}`,
        ...(stillReserved ? [deploymentCancelScope(pending.principal, pending.scope)] : []),
      ].join("\n"));
      await tdReload(mark);
    }, deploymentExplain);
    // Choosing a file replaces the pasted text (and the other way round); any edit makes the next submission a new request.
    for (const p of TD_PROFILES) {
      $(TD(`${p}-file`)).addEventListener("change", () => {
        const file = $(TD(`${p}-file`)).files?.[0];
        if (file) $(TD(`${p}-text`)).value = "";
        $(TD(`${p}-note`)).textContent = file ? `Using the file ${file.name} (${file.size} bytes) exactly as it is. Its SHA-256 appears in the confirmation before anything is sent.` : "";
      });
      $(TD(`${p}-text`)).addEventListener("input", () => {
        if ($(TD(`${p}-file`)).files?.length) { $(TD(`${p}-file`)).value = ""; $(TD(`${p}-note`)).textContent = "Using the pasted text instead of the file."; }
      });
    }
    for (const [form, ids] of Object.entries(TD_FORM_FIELDS)) {
      for (const id of ids) for (const type of ["input", "change"]) $(id).addEventListener(type, () => { delete tdPending[form]; });
    }
    $(TD("readers-mode")).addEventListener("change", () => { $(TD("readers-present")).hidden = $(TD("readers-mode")).value !== "present"; });
    $(TD("operation")).addEventListener("input", () => {
      if (tdCancelPending?.operation !== tdOperationField()) tdCancelPending = null;
      gateTenantDeployments();
    });
    function gate(offline = false) {
      gates = availability(ctx.capabilities(), !offline && ctx.connected());
      $("replication-export").disabled = !gates.replication.allowed;
      $("replication-download").disabled = !gates.replication.allowed || segment === null;
      gateCompaction(offline);
      gateCloudBackup(offline);
      gateAutoFailover(offline);
      gateTenantDeployments(offline);
    }
    function reset() {
      epoch++;
      segment = null;
      known = new Map();
      clearTimeout(timer); timer = null;
      cstatus = null; cview = compactionView(null); dirty = false; pending = null; failures = 0; watchedJob = null;
      $("compaction-status").textContent = "Compaction status not loaded.";
      cList("compaction-facts", [], () => null);
      cList("compaction-reasons", [], () => null);
      cbState = null; cbView = cloudBackupView(null); cbDirty = false; cbSets = null;
      $("cloud-backup-status").textContent = cbView.headline;
      $("cloud-backup-status").setAttribute?.("data-tone", cbView.tone);
      for (const id of ["cloud-backup-facts", "cloud-backup-reasons", "cloud-backup-set-facts"]) cList(id, [], () => null);
      for (const id of ["cloud-backup-set-id", "cloud-backup-pin-id"]) { $(id).replaceChildren?.(); $(id).value = ""; }
      $("cloud-backup-endpoint-note").textContent = ""; $("cloud-backup-endpoint-note").hidden = true;
      cloudFill(null);
      $("cloud-backup-pin-reason").value = ""; cloudFieldError("cloud-backup-pin-reason", "");
      $("cloud-backup-force").checked = false;
      afState = null; afView = autoFailoverView(null);
      $("auto-failover-status").textContent = afView.headline;
      $("auto-failover-status").setAttribute?.("data-tone", afView.tone);
      cList("auto-failover-facts", [], () => null);
      tdAfter = null; tdPage = 0;
      tdStatus("Tenant deployment operations not loaded.", "idle");
      cList("tenant-deployment-list", [], () => null);
      tdPending = {}; tdCancelPending = null; tdOp = null;
      $(TD("operation-status")).textContent = "No operation selected.";
      cList(TD("operation-facts"), [], () => null);
      for (const id of Object.values(TD_MESSAGES)) show(id, "");
      graphSupport(false);
      for (const id of Object.values(OUTPUTS)) show(id, "");
    }
    return {
      capabilities(caps) {
        reset();
        // Saved cancels are used only by the account the server verified now; unreadable (older or damaged) ones are deleted.
        tdPrincipal = tdLastPrincipal = deploymentPrincipal(caps);
        tdCancelSweep(() => false);
        gates = availability(caps, ctx.connected());
        for (const [area, id] of Object.entries(OUTPUTS)) if (!gates[area].allowed) show(id, gates[area].reason);
        gate();
        if (gates.compaction.allowed) cRefresh(epoch, true);
      },
      // sessionEnded: the account signed out or its session was found to have ended, so its saved cancels are deleted.
      // Otherwise (another sign-in replacing this one, or the page closing) they stay for that account only.
      // rejectedToken: the server refused the access token this page sent (app.js passes it only as true). The page may
      // not have learned whose it was (refused before capabilities answered), so it counts as the bootstrap admin
      // token's session ending too. Either way, keys whose owner cannot be read are deleted with them.
      disconnect(options = {}) {
        if (options?.sessionEnded === true) {
          const who = tdPrincipal ?? tdLastPrincipal;
          const token = options.rejectedToken === true;
          tdCancelSweep((owner) => owner === who || (token && owner === "admin_token"));
          tdLastPrincipal = null;
        }
        tdPrincipal = null;
        reset();
        $("replication-after").value = "";
        $("user-password").value = "";
        // Sign-out forgets everything typed or chosen in the tenant deployment forms.
        $(TD("operation")).value = "";
        for (const form of Object.keys(TD_FORM_FIELDS)) tdClearForm(form);
        gate(true);
      },
      // This browser's password session ended (or none exists), so every user account's saved cancels are deleted.
      // The bootstrap admin token's are not: they end with its own sign-out or rejection.
      forgetAccountCancels() { tdCancelSweep((owner) => owner.startsWith("user:")); },
      gate,
    };
  }

  const api = { init, utils: { tenantName, maxVectors, tenantId, tenantRequest, tenantPath, tenantList, tenantView, tenantSettings, tenantUsage, identity, exportPath, exportSegment, exportFilename, availability, explain, utf8Length,
    username, password, userRole, userTenant, userId, userRequest, userChanges, userView, userList,
    formatBytes, compactionForm, compactionSettings, compactionOutcome, compactionView, idempotencyKey, COMPACTION_PHASES,
    cloudBackupForm, cloudBackupPolicy, cloudBackupView, cloudBackupSets, cloudBackupSetFacts, cloudRunOutcome, cloudRestoreOutcome, cloudInterval, cloudCollectionText, cloudDrillText,
    autoFailoverView, autoFailoverRequest, deploymentListPath, deploymentRequest, deploymentView, deploymentList, deploymentBlocked, deploymentExplain,
    sha256Hex, deploymentContent, deploymentCreateSubmission, deploymentReadersSubmission, deploymentRetireSubmission, deploymentTerminal,
    deploymentCancelRequest, deploymentCancelConfirm, deploymentRetryable, deploymentProfileText, DEPLOY_HINTS, DEPLOY_SETTLED,
    deploymentReleasePending, deploymentReleaseConfirm, deploymentSavedCancel, deploymentAck, DEPLOY_PHASES,     DEPLOY_KINDS, RELEASE_PENDING, RELEASE_LOST, RELEASE_REFUSED,
    deploymentPrincipal, deploymentApiScope, deploymentCancelOwner, deploymentCancelScope, DEPLOY_RETRY_AFTER_FIX } };
  if (typeof window !== "undefined") window.VLiteAdminTools = api;
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})();
