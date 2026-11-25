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

  // ---- Tenant deployments (Administration > Users): read-only list of recorded operations (EPIC-011) ----
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
    read_only: "Read-only credentials cannot view tenant deployments. Sign in as a super admin.",
    forbidden_scope: "This credential cannot use admin routes. Sign in as a super admin.",
    gateway_tenant_required: "The HA gateway does not serve admin routes. Sign in to the server directly to view tenant deployments.",
    not_found: "This server does not offer tenant deployments (an older version, or the HA gateway). Sign in to an up-to-date server directly.",
  };
  const DEPLOY_HINTS = {
    ...DEPLOY_BLOCKED,
    admin_auth_required: "Sign in with a super-admin account or the bootstrap admin token.",
    invalid_query: "The page request was not accepted. Select Load operations to start again from the first page.",
    operation_store_unavailable: "The operation store could not be read. Nothing changed; try again shortly.",
    operation_indeterminate: "The operation store did not give a definite answer. Nothing changed; try again shortly.",
    operation_state_invalid: "A stored operation record is damaged. Nothing changed; check the server log.",
  };
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
    return {
      headline: `${deployLabel(DEPLOY_KINDS, o.kind)}: ${deployLabel(DEPLOY_PHASES, o.phase)}`,
      facts: [
        ["Request", deploymentRequest(o.kind, o.desired)],
        ["Next step", text(o.next_step)],
        ["Requested", utcTime(o.created_unix_ms)],
        ["Operation ID", text(o.operation_id)],
        ["Tenant ID", text(o.tenant_id)],
        ["Recorded steps", steps],
      ],
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
    const hint = error?.status === 404 ? DEPLOY_BLOCKED.not_found
      : code && Object.hasOwn(DEPLOY_HINTS, code) ? DEPLOY_HINTS[code]
      : code ? "This view only reads, so nothing changed. Select Load operations to try again; if it keeps failing, check the server log for this code."
      : "";
    return hint ? `${message}\n${hint}` : message;
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
      "tenant-deployment-status", "tenant-deployment-list", "tenant-deployment-refresh", "tenant-deployment-next", "tenant-deployment-help", "tenant-deployment-result"];
    for (const name of controls) if (!$(name)) throw new Error(`admin markup is missing control #${name}`);
    const OUTPUTS = { users: "user-result", tenants: "tenant-result", tls: "tls-result", replication: "replication-result", compaction: "compaction-result", cloudBackup: "cloud-backup-result", autoFailover: "auto-failover-result", tenantDeployments: "tenant-deployment-result" };
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
    // Tenant deployments: read-only, paged by operation ID. Nothing loads until Load operations.
    let tdAfter = null, tdPage = 0;
    function tdStatus(text, tone) {
      $("tenant-deployment-status").textContent = text;
      $("tenant-deployment-status").setAttribute?.("data-tone", tone);
    }
    const tdItem = (view) => {
      const item = document.createElement("li"), title = document.createElement("strong"), facts = document.createElement("dl");
      title.textContent = view.headline;
      facts.className = "compaction-facts";
      facts.append(...view.facts.map(factRow));
      item.append(title, facts);
      return item;
    };
    function gateTenantDeployments(offline = false) {
      const allowed = !offline && gates.tenantDeployments.allowed;
      $("tenant-deployment-refresh").disabled = !allowed;
      $("tenant-deployment-next").disabled = !allowed || tdAfter === null;
      $("tenant-deployment-help").textContent = !allowed ? gates.tenantDeployments.reason
        : tdAfter !== null ? "Next page shows the operations after the last one listed."
        : `Load operations shows the first ${DEPLOY_PAGE}. Nothing loads automatically.`;
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
    on("tenant-deployment-refresh", "tenantDeployments", (mark) => tdLoad(mark, null), deploymentExplain);
    on("tenant-deployment-next", "tenantDeployments", async (mark) => {
      if (tdAfter === null) throw new Error("Select Load operations first.");
      await tdLoad(mark, tdAfter);
    }, deploymentExplain);
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
      graphSupport(false);
      for (const id of Object.values(OUTPUTS)) show(id, "");
    }
    return {
      capabilities(caps) {
        reset();
        gates = availability(caps, ctx.connected());
        for (const [area, id] of Object.entries(OUTPUTS)) if (!gates[area].allowed) show(id, gates[area].reason);
        gate();
        if (gates.compaction.allowed) cRefresh(epoch, true);
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
    username, password, userRole, userTenant, userId, userRequest, userChanges, userView, userList,
    formatBytes, compactionForm, compactionSettings, compactionOutcome, compactionView, idempotencyKey, COMPACTION_PHASES,
    cloudBackupForm, cloudBackupPolicy, cloudBackupView, cloudBackupSets, cloudBackupSetFacts, cloudRunOutcome, cloudRestoreOutcome, cloudInterval, cloudCollectionText, cloudDrillText,
    autoFailoverView, autoFailoverRequest, deploymentListPath, deploymentRequest, deploymentView, deploymentList, deploymentBlocked, deploymentExplain } };
  if (typeof window !== "undefined") window.VLiteAdminTools = api;
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})();
