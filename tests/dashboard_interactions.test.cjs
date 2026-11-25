"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const { test } = require("node:test");

// options.apiBase: the content of <meta name="vlite-api-base"> (the Docker image serves the API under /api); absent by default.
function dashboard(fetch = async () => { throw new Error("Unexpected request"); }, options = {}) {
  class Element {
    constructor(id = "", tag = "DIV") {
      this.id = id; this.tagName = tag; this.dataset = {}; this.attributes = new Map();
      this.value = ""; this.textContent = ""; this.disabled = false; this.inert = false;
      this.children = []; this.selectedOptions = []; this.listeners = {}; this.queries = new Map();
    }
    setAttribute(name, value) { this.attributes.set(name, String(value)); }
    getAttribute(name) { return this.attributes.get(name) ?? null; }
    removeAttribute(name) { this.attributes.delete(name); }
    hasAttribute(name) { return this.attributes.has(name); }
    matches(selector) {
      return selector.split(",").some((part) => {
        part = part.trim();
        if (part === "[data-admin]") return this.hasAttribute("data-admin");
        if (part === "[data-write]") return this.hasAttribute("data-write");
        if (part === "[data-op]") return this.hasAttribute("data-op");
        if (part === "[data-feature]") return this.hasAttribute("data-feature");
        return part === `#${this.id}`;
      });
    }
    closest(selector) {
      if (this.parent?.matches(selector)) return this.parent;
      return this.matches(selector) ? this : null;
    }
    addEventListener(type, listener) { (this.listeners[type] ||= []).push(listener); }
    emit(type) { return Promise.all((this.listeners[type] || []).map((listener) => listener({ target: this }))); }
    querySelector(selector) {
      if (!this.queries.has(selector)) this.queries.set(selector, new Element());
      return this.queries.get(selector);
    }
    querySelectorAll() { return []; }
    replaceChildren(...children) { this.children = children; this.textContent = ""; }
    append(...children) { this.children.push(...children); }
    showModal() { this.open = true; }
    scrollIntoView() {}
    focus() {}
  }
  const elements = new Map();
  const get = (id) => {
    if (!elements.has(id)) elements.set(id, new Element(id));
    return elements.get(id);
  };
  const controls = [];
  const document = {
    activeElement: null,
    getElementById: get,
    querySelector: (selector) => selector === 'meta[name="vlite-api-base"]' && options.apiBase !== undefined ? { content: options.apiBase } : null,
    querySelectorAll(selector) {
      if (selector === "button, input, select, textarea") return controls;
      if (selector === '[data-interaction-gate="blocked"]') return controls.filter((element) => element.dataset.interactionGate === "blocked");
      return controls.filter((element) => element.matches(selector));
    },
    addEventListener() {},
    createElement: (tag) => new Element("", tag.toUpperCase()),
  };
  let context;
  let adminContext;
  const tools = { gate() {}, clear() {}, loaded() {}, connected() {} };
  const adminCalls = [];
  // Options are copied into this realm (the dashboard runs in its own vm context), so deepStrictEqual can compare them.
  const adminTools = { gate() {}, disconnect(options) { adminCalls.push(["disconnect", options === undefined ? undefined : { ...options }]); }, capabilities() {},
    forgetAccountCancels() { adminCalls.push(["forgetAccountCancels"]); } };
  const windowListeners = {};
  const window = {
    addEventListener(type, listener) { (windowListeners[type] ||= []).push(listener); },
    VLiteAdminTools: {
      init(value) { adminContext = value; return adminTools; },
    },
    VLiteTools: {
      utils: {
        collectionPath: (name, suffix) => name === "default" ? `/v1${suffix}` : `/v1/collections/${name}${suffix}`,
        collectionName: (name) => name,
        isSearch: (path) => path.endsWith("/search"),
        redact: (value) => value,
        id() {}, rerank: () => ({}),
        scoreDirection: () => ({ label: "Distance; lower is better" }),
        serverTime: () => null,
      },
      init: (value) => {
        context = value;
        return tools;
      },
    },
  };
  const source = fs.readFileSync("src/dashboard/app.js", "utf8").replace(/\}\)\(\);\s*$/, `
    window.testDashboard = {
      payloadPreview, metadataPreview, vectorPreview, presentRecord, confirmOperation,
      // credential "" signs in with the password session cookie instead of an access token.
      setConnection(name = "default", credential = "memory-only-credential") {
        connected = true; token = credential; collection = name; dimensions = 2;
        capabilities = { role: "admin", features: { collections: true }, format_version: 4 };
      },
      setCapabilities(value) { capabilities = value; },
      setSession(value) { session = value; },
      clearRecordInspector, advanceWorkflowContext, connect, memoryOff, restoreSession
    };
  })();`);
  vm.runInNewContext(source, {
    window, document, fetch, AbortController, TextDecoder, Uint8Array, atob,
    performance, setTimeout, clearTimeout, Option: Element,
  });
  get("mode").value = "exact";
  get("metric").value = "squared_l2";
  get("hnsw-m").value = "16"; get("hnsw-ef").value = "100"; get("hnsw-seed").value = "42";
  get("k").value = "10"; get("query").value = "[1,0]"; get("filter").value = "[]";
  window.testDashboard.setConnection();
  return { ...window.testDashboard, context, adminContext, tools, adminTools, adminCalls, windowListeners, window, document, get, controls, Element };
}

const response = (value, status = 200) => ({
  ok: status < 400, status, headers: { get: () => null },
  text: async () => JSON.stringify(value), json: async () => value,
});
const deferred = () => {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
};

test("both dashboard modules initialize with the same transport context", () => {
  const ui = dashboard();
  assert.equal(ui.adminContext, ui.context);
  assert.doesNotMatch(ui.get("status").textContent, /failed to initialize/);
});

test("the admin tools get the API base the requests use, so saved cancels are filed under that address", async () => {
  assert.equal(dashboard().adminContext.apiBase, "");
  const urls = [];
  const ui = dashboard(async (url) => { urls.push(url); return response({ id: 1 }); }, { apiBase: "/api" });
  assert.equal(ui.adminContext.apiBase, "/api");
  await ui.context.api("GET", "/v1/vectors/1");
  assert.deepEqual(urls, ["/api/v1/vectors/1"]);
});

test("payload previews decode UTF-8 safely, bound text and explicitly identify binary data", () => {
  const ui = dashboard();
  const encode = (value) => Buffer.from(value).toString("base64");
  assert.equal(ui.payloadPreview(encode("<script>雪</script>")), "<script>雪</script>");
  assert.equal(ui.payloadPreview(""), "(empty payload)");
  for (const value of ["AP8=", "AAAA", "!"]) assert.match(ui.payloadPreview(value), /Binary|invalid UTF-8/);
  assert.match(ui.payloadPreview(undefined), /not included/);
  assert.match(ui.payloadPreview(encode("a".repeat(20)), 8), /^aaaaaaaa\n… Preview truncated/);
});

test("metadata and vector previews retain type information with bounded vector components", () => {
  const ui = dashboard();
  const metadata = JSON.parse('{"__proto__":{"type":"string","value":"<unsafe>"},"count":{"type":"integer","value":"18446744073709551615"}}');
  assert.match(ui.metadataPreview(metadata), /__proto__: <unsafe> \(string\)/);
  assert.match(ui.metadataPreview(metadata, ["count"]), /^count: 18446744073709551615 \(integer\)$/);
  assert.match(ui.vectorPreview(Array.from({ length: 1024 }, (_, index) => index)), /^1024 components; first 32 shown/);
  assert.ok(!ui.vectorPreview(Array.from({ length: 1024 }, (_, index) => index)).includes(",32"));
  assert.match(ui.vectorPreview(null), /not included/);
});

test("inspector renders text-only panels, bounded vector and complete raw record without losing legacy output", () => {
  const ui = dashboard();
  const record = { id: "42", payload_base64: Buffer.from("<img src=x>").toString("base64"), metadata: {}, vector: Array(100).fill(1) };
  ui.presentRecord(record);
  assert.equal(ui.get("record-inspector-payload").textContent, "<img src=x>");
  assert.match(ui.get("record-inspector-vector").textContent, /first 32/);
  assert.equal(JSON.parse(ui.get("record-inspector-raw-content").textContent).vector.length, 100);
  assert.match(ui.get("browse-preview").textContent, /Record 42.*collection default/);
  assert.match(ui.get("browse-preview").textContent, /Payload preview/);
  ui.clearRecordInspector();
  assert.match(ui.get("record-inspector-raw-content").textContent, /Select a record/);
});

test("workflows serialize while navigation stays interactive and semantic disabled gates survive", async () => {
  const ui = dashboard();
  const work = new ui.Element("search-submit", "BUTTON");
  const disabled = new ui.Element("unavailable-feature", "BUTTON"); disabled.disabled = true;
  const navigation = new ui.Element("navigation-toggle", "BUTTON");
  const theme = new ui.Element("theme", "SELECT");
  ui.controls.push(work, disabled, navigation, theme);
  ui.document.activeElement = work;
  const pending = deferred();
  let calls = 0;
  const first = ui.context.run(async () => { calls++; await pending.promise; });
  assert.equal(work.inert, true);
  assert.equal(work.getAttribute("aria-disabled"), "true");
  assert.equal(disabled.disabled, true);
  assert.equal(navigation.inert, false);
  assert.equal(theme.inert, false);
  await ui.context.run(async () => { calls++; });
  assert.equal(calls, 1);
  pending.resolve();
  await first;
  assert.equal(work.inert, false);
  assert.equal(work.getAttribute("aria-disabled"), null);
  assert.equal(disabled.disabled, true);
});

test("authorization gates stay disabled after a workflow and can recover on an authorized reconnect", async () => {
  const ui = dashboard();
  const control = new ui.Element("admin-action", "BUTTON");
  control.setAttribute("data-admin", "");
  ui.controls.push(control);
  ui.setCapabilities({ role: "read_only", features: {} });
  await ui.context.run(async () => {});
  assert.equal(control.disabled, true);
  ui.setCapabilities({ role: "admin", features: {} });
  await ui.context.run(async () => {});
  assert.equal(control.disabled, false);
});

test("dynamic feed/job/pagination gates supersede their pre-operation disabled state", async () => {
  const ui = dashboard();
  const controls = ["feed-next", "job-resume", "next-page"].map((id) => {
    const element = new ui.Element(id, "BUTTON");
    element.disabled = true;
    return element;
  });
  ui.controls.push(...controls);
  ui.tools.gate = () => { controls.forEach((element) => { element.disabled = false; }); };
  await ui.context.run(async () => {});
  for (const control of controls) {
    assert.equal(control.disabled, false, `${control.id} must reflect successful workflow state`);
    assert.equal(control.inert, false);
  }
  ui.tools.gate = () => { controls.forEach((element) => { element.disabled = true; }); };
  await ui.context.run(async () => {});
  for (const control of controls) assert.equal(control.disabled, true, `${control.id} must remain state-gated`);
});

test("workflow direct enable is not overwritten by restoring a disabled snapshot", async () => {
  const ui = dashboard();
  const control = new ui.Element("feed-next", "BUTTON");
  control.disabled = true;
  ui.controls.push(control);
  await ui.context.run(async () => { control.disabled = false; });
  assert.equal(control.disabled, false);
  assert.equal(control.inert, false);
});

test("disconnect discards a late foreground response even when fetch ignores abort", async () => {
  const pending = deferred();
  const ui = dashboard(() => pending.promise);
  const operation = ui.context.run(async () => {
    const result = await ui.context.api("GET", "/v1/vectors/1");
    ui.get("record").textContent = result.secret;
  });
  ui.context.disconnect();
  pending.resolve(response({ secret: "must-not-reappear" }));
  await operation;
  assert.equal(ui.get("record").textContent, "");
  assert.equal(ui.get("inspector").textContent, "");
  assert.match(ui.get("status").textContent, /Signed out/);
});

test("disconnect invalidates pending confirmations and prevents subsequent requests", async () => {
  const ui = dashboard();
  const answer = ui.confirmOperation("Replace record?");
  ui.context.disconnect();
  ui.get("confirmation").returnValue = "confirm";
  await ui.get("confirmation").emit("close");
  assert.equal(await answer, false);
});

test("a private submission sends the original body but the inspector only shows its summary", async () => {
  const body = { contents: ["private deployment settings"], request: { secret_versions: ["private reference"] } };
  const summary = { documents: 1, secret_versions: 1, profile_digests: ["a".repeat(64)] };
  const pending = deferred();
  let sent;
  const ui = dashboard((_url, options) => { sent = JSON.parse(options.body); return pending.promise; });
  const send = () => ui.context.api("POST", "/v1/admin/tenant-deployments", body, { data: false, inspectorSummary: summary });
  const request = send();
  const inspector = ui.get("inspector");
  assert.deepEqual(sent, body);
  assert.deepEqual(JSON.parse(inspector.textContent).request.body, summary);
  pending.resolve(response({ operation: "ok", echoed: body }));
  assert.equal((await request).operation, "ok");
  assert.deepEqual(JSON.parse(inspector.textContent).request.body, summary);
  assert.doesNotMatch(inspector.textContent, /private deployment settings|private reference/);
});

test("a 401 clears the inspector without exposing the previous request or processing the response body", async () => {
  const ui = dashboard(async () => ({ status: 401, ok: false, text() { throw new Error("not JSON"); } }));
  await assert.rejects(ui.context.api("POST", "/v1/admin/tenant-deployments", { contents: ["private settings"] }),
    (e) => e.status === 401);
  assert.equal(ui.get("inspector").textContent, "");
  assert.equal(ui.context.connected(), false);
  assert.match(ui.get("gate-message").textContent, /HTTP 401/);
  assert.doesNotMatch(ui.get("gate-message").textContent, /private settings|not JSON/);
});

test("sign-out and an ended session delete saved tenant-deployment cancels; leaving the page or reconnecting keeps them", async () => {
  // Both sign-out buttons (and a credentials reload) call disconnect(): the signed-in account's saved cancels go.
  let ui = dashboard();
  ui.adminCalls.length = 0;
  ui.context.disconnect();
  assert.deepEqual(ui.adminCalls, [["disconnect", { sessionEnded: true }]]);
  // Leaving the page (pagehide) keeps them, so the same account can resend after a reload.
  ui = dashboard();
  ui.adminCalls.length = 0;
  for (const listener of ui.windowListeners.pagehide) listener({});
  assert.deepEqual(ui.adminCalls, [["disconnect", { sessionEnded: false }]]);
  // A 401 (session expired or revoked, token rejected) ends the session. A rejected access token also tells the admin
  // tools so, which deletes the bootstrap admin token's saved cancels even before the page learned who was signed in.
  ui = dashboard(async () => response({ error: { code: "unauthorized", message: "session ended" } }, 401));
  ui.adminCalls.length = 0;
  await assert.rejects(ui.context.api("GET", "/v1/vectors/1"), (e) => e.status === 401);
  assert.deepEqual(ui.adminCalls, [["disconnect", { sessionEnded: true, rejectedToken: true }]]);
  // A password session (no access token) found ended is not a rejected token.
  ui = dashboard(async () => response({ error: { code: "unauthorized", message: "session ended" } }, 401));
  ui.setConnection("default", "");
  ui.setSession({ username: "alice", mustChange: false });
  ui.adminCalls.length = 0;
  await assert.rejects(ui.context.api("GET", "/v1/vectors/1"), (e) => e.status === 401);
  assert.deepEqual(ui.adminCalls, [["disconnect", { sessionEnded: true }]]);
  // disconnect() passes rejectedToken on only when it is exactly true.
  for (const [options, expected] of [
    [{ endSession: false, rejectedToken: true }, { sessionEnded: true, rejectedToken: true }],
    [{ endSession: false, rejectedToken: false }, { sessionEnded: true }],
    [{ endSession: false, rejectedToken: "yes" }, { sessionEnded: true }],
    [{ endSession: false, rejectedToken: 1 }, { sessionEnded: true }],
    [{ endSession: false }, { sessionEnded: true }],
    [{ endSession: false, leaving: true }, { sessionEnded: false }],
  ]) {
    ui = dashboard();
    ui.adminCalls.length = 0;
    ui.context.disconnect(options);
    assert.deepEqual(ui.adminCalls, [["disconnect", expected]], JSON.stringify(options));
  }
  // Reconnecting (Refresh session, Connect) only clears the page; a reconnect that cannot reach the server deletes nothing.
  ui = dashboard(async () => { throw new Error("offline"); });
  ui.adminCalls.length = 0;
  await ui.context.run(ui.connect);
  assert.deepEqual(ui.adminCalls, [["disconnect", undefined]]);
});

test("a typed token that replaces a password session deletes the accounts' saved cancels first", async () => {
  const ui = dashboard(async () => { throw new Error("offline"); });
  ui.context.disconnect();
  ui.setSession({ username: "alice", mustChange: false });
  ui.adminCalls.length = 0;
  ui.get("token").value = "typed-token";
  ui.get("connection").listeners.submit[0]({ preventDefault() {} });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(ui.adminCalls, [["forgetAccountCancels"], ["disconnect", undefined]]);
});

test("a typed token refused on a fresh page, even with a non-JSON 401, is reported as a rejected token", async () => {
  const urls = [];
  const ui = dashboard(async (url) => {
    urls.push(url);
    return { ok: false, status: 401, headers: { get: () => null }, text: async () => "<html>401 Authorization Required</html>" };
  });
  // A fresh page: nothing signed in, nothing known about the account yet.
  ui.context.disconnect();
  ui.adminCalls.length = 0;
  ui.get("token").value = "mistyped-token";
  ui.get("connection").listeners.submit[0]({ preventDefault() {} });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(urls, ["/v1/capabilities"]);
  // connect() clears the page (no options), then the refusal ends the session as a rejected token.
  assert.deepEqual(ui.adminCalls, [["disconnect", undefined], ["disconnect", { sessionEnded: true, rejectedToken: true }]]);
  assert.equal(ui.context.connected(), false);
  assert.equal(ui.context.tokenAuth(), false);
});

test("a page load with no password session deletes the accounts' saved cancels; an unreadable answer deletes nothing", async () => {
  const auth = (config, current) => async (url) => {
    const route = url.split("?")[0];
    if (route === "/v1/auth/config") return config();
    if (route === "/v1/auth/session") return current();
    throw new Error(`Unexpected ${route}`);
  };
  const offline = () => { throw new Error("offline"); };
  const on = () => response({ password_login: true });
  for (const [label, config, current, forgets] of [
    ["password sign-in is off", () => response({ password_login: false }), offline, true],
    ["session cookie expired or absent", on, () => response({ error: { code: "unauthorized", message: "x" } }, 401), true],
    ["no password session", on, () => response({ method: "open" }), true],
    ["config unreachable", offline, offline, false],
    ["session unreachable", on, offline, false],
    ["session busy", on, () => response({ error: { code: "busy", message: "x" } }, 503), false],
  ]) {
    const ui = dashboard(auth(config, current));
    ui.context.disconnect();
    ui.adminCalls.length = 0;
    await ui.restoreSession();
    assert.deepEqual(ui.adminCalls, forgets ? [["forgetAccountCancels"]] : [], label);
  }
  // A live password session resumes the same account (connect), so its saved cancels stay usable.
  const ui = dashboard(auth(on, () => response({ method: "password", user: { username: "alice" } })));
  ui.context.disconnect();
  ui.adminCalls.length = 0;
  await ui.restoreSession();
  assert.deepEqual(ui.adminCalls, [["disconnect", undefined]]);
});

test("independent observability requests snapshot context and discard stale completions", async () => {
  const pending = deferred();
  let signal;
  const ui = dashboard((_route, options) => { signal = options.signal; return pending.promise; });
  const request = ui.window.VLiteObservabilityConnection.request("/metrics");
  ui.context.disconnect();
  assert.equal(signal.aborted, true);
  pending.resolve(response({ secret: "late" }));
  await assert.rejects(request, /response discarded/);
});

test("same-collection reconnect invalidates independent reads as well as disconnect", async () => {
  const pending = deferred();
  const ui = dashboard(() => pending.promise);
  const request = ui.window.VLiteObservabilityConnection.request("/metrics");
  ui.advanceWorkflowContext();
  pending.resolve(response({ stale: true }));
  await assert.rejects(request, /response discarded/);
});

test("Stop waiting explains that aborting a browser wait does not cancel a mutation", async () => {
  const ui = dashboard((_route, options) => new Promise((_resolve, reject) => {
    options.signal.addEventListener("abort", () => reject(new Error("aborted")));
  }));
  const operation = ui.context.run(() => ui.context.api("PUT", "/v1/vectors/1", { vector: [1, 0] }));
  await ui.get("cancel-request").emit("click");
  await operation;
  assert.match(ui.get("status").textContent, /server work was not cancelled/);
  assert.match(ui.get("status").textContent, /Mutation outcome may be unknown/);
});

test("a data:false admin POST keeps browse, search and inspector state on 200, 500 and 503", async () => {
  const replies = [response({ outcome: "unchanged" }), response({ error: { code: "internal_error", message: "x" } }, 500),
    response({ error: { code: "busy", message: "database queue is full" } }, 503)];
  const ui = dashboard(async () => replies.shift());
  const preflight = () => ui.context.api("POST", "/v1/admin/memory/bindings/preflight", {}, { data: false });
  const kept = new ui.Element("kept-row");
  const marks = () => {
    ui.get("index-status").textContent = "Index ready";
    ui.get("browse-preview").textContent = "Record 42";
    ui.get("payload-preview").textContent = "payload";
    ui.get("hits").querySelector("tbody").replaceChildren(kept);
  };
  const assertKept = () => {
    assert.equal(ui.get("index-status").textContent, "Index ready");
    assert.doesNotMatch(ui.get("index-status").textContent, /Data changed/);
    assert.equal(ui.get("browse-preview").textContent, "Record 42");
    assert.equal(ui.get("payload-preview").textContent, "payload");
    assert.deepEqual(ui.get("hits").querySelector("tbody").children, [kept]);
  };
  marks();
  assert.equal(JSON.stringify(await preflight()), JSON.stringify({ outcome: "unchanged" }));
  assertKept();
  for (const status of [500, 503]) {
    marks();
    await assert.rejects(preflight(), (error) => error.status === status);
    assertKept();
  }
  // The control: the same POST without data:false is a mutation and clears.
  const mutation = dashboard(async () => response({ outcome: "unchanged" }));
  mutation.get("browse-preview").textContent = "Record 42";
  await mutation.context.api("POST", "/v1/admin/memory/bindings/preflight", {});
  assert.match(mutation.get("index-status").textContent, /Data changed/);
  assert.equal(mutation.get("browse-preview").textContent, "");
});

test("search preserves existing rows during refresh and after read failures", async () => {
  const pending = deferred();
  let calls = 0;
  const ui = dashboard(() => ++calls === 1 ? Promise.resolve(response({ hits: [{ id: "9", distance: 0.25 }] })) : pending.promise);
  await ui.context.run(ui.context.search);
  const table = ui.get("hits").querySelector("tbody");
  const previous = table.children[0];
  const operation = ui.context.run(ui.context.search);
  assert.equal(table.children[0], previous);
  assert.match(ui.get("search-summary").textContent, /Previous results remain/);
  assert.equal(ui.get("hits").getAttribute("aria-busy"), "true");
  pending.resolve(response({ error: { code: "unavailable", message: "read failed" } }, 503));
  await operation;
  assert.equal(table.children[0], previous);
  assert.equal(ui.get("hits").getAttribute("aria-busy"), null);
  assert.match(ui.get("search-summary").textContent, /Refresh failed; previous results retained/);
});

test("successful searches clearly label cosine and squared L2 as distance, never similarity", async () => {
  for (const metric of ["cosine", "squared_l2"]) {
    const ui = dashboard(async () => response({ hits: [{ id: "9", distance: 0.25 }] }));
    ui.get("metric").value = metric;
    await ui.context.run(ui.context.search);
    const headers = ui.get("hits").querySelector("thead tr").children;
    assert.ok(headers.some((header) => header.textContent === "Distance (lower is better)"));
    assert.match(ui.get("hits").querySelector("caption").textContent, /lower distance is better/);
    assert.equal(ui.get("hits").querySelector("tbody").children.length, 1);
  }
});

const refusal = () => response({ error: { code: "forbidden_scope", message: "credential scope does not permit this operation or collection" } }, 403);
const MEMORY_OFF_METRICS = "Server metrics are off while agent memory is enabled.";

// The 403 forbidden_scope that agent memory (docs/SECURITY.md, Memory-bound resources) makes the server answer to /metrics.
function signInServer(role, overrides = {}, scope = undefined) {
  const calls = [];
  const routes = {
    "/v1/capabilities": () => response({ api_version: 1, role, format_version: 5, features: {}, ...(scope ? { scope } : {}), identity: { kind: "super_admin" } }),
    "/v1/collections": () => response({ items: [], format_version: 5 }),
    "/v1/database": () => response({ dimensions: 3, records: 0, format_version: 5, is_encrypted: false }),
    "/metrics": refusal,
    ...overrides,
  };
  return { calls, fetch: async (url) => { const route = url.split("?")[0]; calls.push(route); if (!routes[route]) throw new Error(`Unexpected ${route}`); return routes[route](); } };
}

test("sign-in survives a refused /metrics while agent memory is enabled and the overview says so", async () => {
  for (const role of ["admin", "read_only"]) {
    const server = signInServer(role);
    const ui = dashboard(server.fetch);
    await ui.context.run(ui.connect);
    assert.match(ui.get("status").textContent, /^Connected; overview refreshed$/);
    assert.equal(ui.get("status").dataset.error, "false");
    assert.equal(ui.get("gate-message").dataset.error, "false");
    assert.equal(ui.get("metrics-note").hidden, false);
    assert.equal(ui.get("metrics-note").textContent, `${MEMORY_OFF_METRICS} Database size shows Unavailable.`);
    assert.equal(ui.get("stat-bytes").textContent, "Unavailable");
    assert.match(ui.get("overview").textContent, /"metrics": "Unavailable: server metrics are off while agent memory is enabled\."/);
    assert.doesNotMatch(`${ui.get("status").textContent}${ui.get("gate-message").textContent}${ui.get("error-help").textContent}`, /403|forbidden_scope/);
    assert.deepEqual(server.calls.slice(0, 4), ["/v1/capabilities", "/v1/collections", "/v1/database", "/metrics"]);
  }
});

test("a server without agent memory keeps showing the metrics figures and no note", async () => {
  const server = signInServer("admin", { "/metrics": () => response({ database_bytes: 4096 }) });
  const ui = dashboard(server.fetch);
  await ui.context.run(ui.connect);
  assert.equal(ui.get("stat-bytes").textContent, "4096");
  assert.equal(ui.get("metrics-note").hidden, true);
  assert.equal(ui.get("metrics-note").textContent, "");
});

test("a scoped credential with diagnostics signs in through a refused /metrics, one without never asks", async () => {
  const scope = (operations) => ({ operations, collections: ["*"], graphs: [], expires_unix_ms: 1 });
  const allowed = signInServer("scoped", {}, scope(["read", "diagnostics"]));
  const ui = dashboard(allowed.fetch);
  await ui.context.run(ui.connect);
  assert.equal(ui.get("status").dataset.error, "false");
  assert.equal(ui.get("metrics-note").hidden, false);
  const without = signInServer("scoped", {}, scope(["read"]));
  const other = dashboard(without.fetch);
  await other.context.run(other.connect);
  assert.equal(other.get("status").dataset.error, "false");
  assert.equal(other.get("metrics-note").hidden, true);
  assert.ok(!without.calls.includes("/metrics"));
});

test("sign-in still fails for every other refusal or failure", async () => {
  const failures = [
    ["/metrics", () => response({ error: { code: "internal_error", message: "boom" } }, 500), /HTTP 500: internal_error: boom/],
    ["/metrics", () => response({ error: { code: "busy", message: "server busy" } }, 503), /HTTP 503: busy/],
    ["/metrics", () => response({ error: { code: "unauthorized", message: "bad token" } }, 401), /HTTP 401: unauthorized/],
    ["/metrics", () => response({ error: { code: "read_only", message: "admin access required" } }, 403), /HTTP 403: read_only/],
    ["/v1/database", refusal, /HTTP 403: forbidden_scope/],
  ];
  for (const [route, answer, message] of failures) {
    const ui = dashboard(signInServer("admin", { [route]: answer }).fetch);
    await ui.context.run(ui.connect);
    assert.equal(ui.get("status").dataset.error, "true", route);
    assert.match(ui.get("status").textContent, message);
    assert.equal(ui.get("metrics-note").hidden, true);
  }
  const offline = dashboard(signInServer("admin", { "/metrics": () => { throw new Error("socket closed"); } }).fetch);
  await offline.context.run(offline.connect);
  assert.equal(offline.get("status").dataset.error, "true");
  assert.match(offline.get("status").textContent, /GET \/metrics failed/);
});

test("only the routes agent memory switches off, and only for credentials that can use them, read as the memory refusal", () => {
  const ui = dashboard();
  const admin = (method, route) => ui.memoryOff(method, route, 403, "forbidden_scope");
  assert.equal(admin("GET", "/metrics?format=prometheus"), MEMORY_OFF_METRICS);
  assert.equal(admin("GET", "/v1/admin/metrics/summary"), MEMORY_OFF_METRICS);
  assert.equal(admin("GET", "/v1/admin/graph/stats"), "Graph engine statistics are off while agent memory is enabled.");
  assert.equal(admin("GET", "/v1/admin/replication/export?max_bytes=1"), "Replication export is off while agent memory is enabled.");
  for (const route of ["/v1/admin/webhooks", "/v1/admin/webhooks/0123456789abcdef01234567"]) assert.equal(admin("GET", route), "Webhook lists and delivery status are off while agent memory is enabled.");
  for (const [method, route] of [["POST", "/v1/collections/docs/imports"], ["GET", "/v1/imports/7"], ["PUT", "/v1/imports/7/chunks/2"], ["POST", "/v1/imports/7/finish"], ["POST", "/v1/imports/7/abort"]]) assert.equal(admin(method, route), "Resumable imports are off while agent memory is enabled.");
  for (const [method, route] of [["GET", "/v1/admin/maintenance/auto"], ["PUT", "/v1/admin/maintenance/auto"], ["POST", "/v1/admin/maintenance/auto/tick"]]) assert.equal(admin(method, route), "Automatic index maintenance is off while agent memory is enabled.");
  // Creating and deleting subscriptions, and every other route, are not refused for this reason.
  for (const [method, route] of [["POST", "/v1/admin/webhooks"], ["DELETE", "/v1/admin/webhooks/0123456789abcdef01234567"], ["GET", "/v1/admin/logs"], ["GET", "/v1/admin/backups"], ["GET", "/v1/vectors/1"], ["GET", "/v1/collections"]]) assert.equal(admin(method, route), "", `${method} ${route}`);
  // Any other status or code is an ordinary failure.
  assert.equal(ui.memoryOff("GET", "/metrics", 403, "read_only"), "");
  assert.equal(ui.memoryOff("GET", "/metrics", 401, "forbidden_scope"), "");
  assert.equal(ui.memoryOff("GET", "/metrics", 500, undefined), "");
  // A read-only token reaches /metrics only; scoped credentials need the operation the route asks for.
  ui.setCapabilities({ role: "read_only", features: {} });
  assert.equal(admin("GET", "/metrics"), MEMORY_OFF_METRICS);
  assert.equal(admin("GET", "/v1/admin/webhooks"), "");
  assert.equal(admin("GET", "/v1/admin/metrics/summary"), "");
  const scoped = (operations, collections = ["*"]) => ui.setCapabilities({ role: "scoped", features: {}, scope: { operations, collections } });
  scoped(["read"]);
  assert.equal(admin("GET", "/metrics"), "");
  assert.equal(admin("POST", "/v1/collections/docs/imports"), "");
  scoped(["read", "diagnostics", "import"]);
  assert.equal(admin("GET", "/metrics"), MEMORY_OFF_METRICS);
  // An import refusal can be an ordinary scope denial (another tenant's job), so a scoped credential never reads as memory.
  for (const [method, route] of [["POST", "/v1/collections/docs/imports"], ["GET", "/v1/imports/7"], ["POST", "/v1/imports/7/finish"]]) assert.equal(admin(method, route), "", `${method} ${route}`);
  assert.equal(admin("GET", "/v1/admin/webhooks"), "");
  assert.equal(admin("GET", "/v1/admin/maintenance/auto"), "");
  // Replication export: the server checks read on every collection and no tenant before it refuses for memory.
  scoped(["read"]);
  assert.equal(admin("GET", "/v1/admin/replication/export"), "Replication export is off while agent memory is enabled.");
  scoped(["read"], ["docs"]);
  assert.equal(admin("GET", "/v1/admin/replication/export"), "");
  scoped(["search"]);
  assert.equal(admin("GET", "/v1/admin/replication/export"), "");
  ui.setCapabilities({ role: "scoped", features: {}, scope: { operations: ["read"], collections: ["*"], tenant_id: "ab12cd34ef56ab12cd34ef56" } });
  assert.equal(admin("GET", "/v1/admin/replication/export"), "");
  // A read-only token reads the import status the same way: it cannot be denied for any ordinary reason.
  ui.setCapabilities({ role: "read_only", features: {} });
  assert.equal(admin("GET", "/v1/imports/7"), "Resumable imports are off while agent memory is enabled.");
  assert.equal(admin("POST", "/v1/imports/7/finish"), "");
  assert.equal(admin("GET", "/v1/admin/replication/export"), "");
});

test("api() reports a memory refusal in plain words and keeps every other 403 as it was", async () => {
  const ui = dashboard(async (url) => url.startsWith("/v1/vectors/") ? refusal() : response({ error: { code: "forbidden_scope", message: "credential scope does not permit this operation or collection" } }, 403));
  await assert.rejects(ui.context.api("GET", "/v1/admin/webhooks"), (error) => {
    assert.equal(error.memoryMode, true);
    assert.equal(error.message, "Webhook lists and delivery status are off while agent memory is enabled.");
    assert.equal(error.unavailable, "Unavailable: webhook lists and delivery status are off while agent memory is enabled.");
    assert.match(error.help, /expected, not a sign-in or permission problem/);
    assert.equal(error.status, 403);
    assert.equal(error.code, "forbidden_scope");
    return true;
  });
  await assert.rejects(ui.context.api("GET", "/v1/vectors/1"), (error) => {
    assert.equal(error.memoryMode, undefined);
    assert.match(error.message, /^HTTP 403: forbidden_scope: /);
    return true;
  });
  // Unhandled, a refusal reads as one plain sentence with its explanation, never as a raw HTTP error.
  await ui.context.run(() => ui.context.api("GET", "/metrics"));
  assert.equal(ui.get("status").textContent, MEMORY_OFF_METRICS);
  assert.match(ui.get("error-help").textContent, /not a sign-in or permission problem/);
});

test("independent observability reads tell the memory refusal from any other 403", async () => {
  let answer = refusal;
  const ui = dashboard(async () => answer());
  await assert.rejects(ui.window.VLiteObservabilityConnection.request("/v1/admin/metrics/summary"), (error) => error.memoryMode === true && error.message === MEMORY_OFF_METRICS && error.status === 403);
  await assert.rejects(ui.window.VLiteObservabilityConnection.request("/metrics", { text: true }), (error) => error.memoryMode === true);
  await assert.rejects(ui.window.VLiteObservabilityConnection.request("/v1/admin/logs"), (error) => error.memoryMode === undefined && error.status === 403 && error.message === "HTTP 403");
  answer = () => response({ error: { code: "read_only", message: "admin access required" } }, 403);
  await assert.rejects(ui.window.VLiteObservabilityConnection.request("/v1/admin/metrics/summary"), (error) => error.memoryMode === undefined && error.status === 403 && error.code === "read_only");
  answer = () => ({ ok: false, status: 403 });
  await assert.rejects(ui.window.VLiteObservabilityConnection.request("/v1/admin/metrics/summary"), (error) => error.memoryMode === undefined && error.status === 403);
});

test("the graph explorer's statistics refusal carries the memory message and nothing else changes", async () => {
  const ui = dashboard(async (url) => url === "/v1/admin/graph/stats" ? refusal() : response({ graphs: [] }));
  const stats = await ui.window.VLiteGraphConnection.request("GET", "/v1/admin/graph/stats");
  assert.equal(stats.status, 403);
  assert.equal(stats.memoryMode, "Graph engine statistics are off while agent memory is enabled.");
  const list = await ui.window.VLiteGraphConnection.request("GET", "/v1/graphs");
  assert.equal(list.status, 200);
  assert.equal(list.memoryMode, undefined);
});
