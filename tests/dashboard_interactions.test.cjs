"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const { test } = require("node:test");

function dashboard(fetch = async () => { throw new Error("Unexpected request"); }) {
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
    querySelector: () => null,
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
  const tools = { gate() {}, clear() {}, loaded() {} };
  const adminTools = { gate() {}, disconnect() {}, capabilities() {} };
  const window = {
    addEventListener() {},
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
      setConnection(name = "default") {
        connected = true; token = "memory-only-credential"; collection = name; dimensions = 2;
        capabilities = { role: "admin", features: { collections: true }, format_version: 4 };
      },
      setCapabilities(value) { capabilities = value; },
      clearRecordInspector, advanceWorkflowContext
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
  return { ...window.testDashboard, context, adminContext, tools, adminTools, window, document, get, controls, Element };
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
