const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { resolveRoute, nextTabIndex, normalizeTarget } = require("../src/dashboard/navigation.js");

const html = fs.readFileSync(path.join(__dirname, "../src/dashboard/index.html"), "utf8");

test("every legacy section hash selects its containing workspace", () => {
  const expected = {
    database: "overview", collections: "collections", browse: "collections", editor: "collections",
    "write-json": "collections", index: "collections", search: "search", retrieval: "search",
    explore: "search", operations: "administration", credentials: "administration", webhooks: "administration",
    tenants: "administration", "transport-security": "administration", replication: "administration",
    observability: "observability", diagnostics: "observability", "change-feed": "observability",
    requests: "developer", "data-tools": "developer", "resumable-import": "developer"
  };
  const sectionIds = Array.from(html.matchAll(/<section\b[^>]*\bid="([^"]+)"/g), match => match[1]);
  assert.deepEqual(sectionIds.slice().sort(), Object.keys(expected).sort());
  for (const [id, workspace] of Object.entries(expected)) {
    assert.deepEqual(resolveRoute(`#${id}`), { id, workspace, section: id, tab: id === "write-json" ? "editor" : id });
  }
});

test("focused admin routes preserve containing sections and parent tabs", () => {
  for (const [id, section, tab] of [
    ["admin-health", "operations", "operations"],
    ["admin-token-management", "operations", "operations"],
    ["admin-backups", "operations", "admin-backups"],
    ["admin-backup-schedule", "operations", "admin-backups"],
    ["admin-auto-maintenance", "operations", "admin-auto-maintenance"],
    ["admin-persistent-audit", "credentials", "credentials"],
    ["admin-webhooks", "webhooks", "webhooks"],
    ["admin-pitr", "webhooks", "webhooks"]
  ]) {
    assert.deepEqual(resolveRoute(`#${id}`), { id, section, tab, workspace: "administration" });
    assert.match(html, new RegExp(`id="${id}"[^>]*data-admin-view`));
  }
});

test("focused admin navigation opens details and legacy reveals restore hidden panels", () => {
  const elements = new Map();
  function element(id, tagName = "DIV") {
    const attributes = new Map();
    const item = {
      id, tagName, hidden: false, open: false, children: [], dataset: {},
      parentElement: null, classList: { toggle() {} },
      setAttribute(name, value) { attributes.set(name, value); },
      getAttribute(name) { return attributes.get(name); },
      removeAttribute(name) { attributes.delete(name); },
      replaceChildren() { this.children = []; },
      append(child) { this.children.push(child); child.parentElement = this; },
      addEventListener() {}, focus() {}, scrollIntoView() {},
      contains() { return false; },
      querySelector(selector) {
        return selector === '[aria-selected="true"]'
          ? this.children.find(child => child.getAttribute("aria-selected") === "true") || null : null;
      },
      closest(selector) {
        for (let node = this; node; node = node.parentElement) {
          if (selector === "main > section" && node.tagName === "SECTION") return node;
          if (selector === "[data-admin-view]" && node.adminView) return node;
        }
        return null;
      }
    };
    if (id) elements.set(id, item);
    return item;
  }
  for (const match of html.matchAll(/\bid="([^"]+)"/g)) element(match[1]);
  const sections = [...html.matchAll(/<section\b[^>]*id="([^"]+)"/g)].map(match => {
    const section = elements.get(match[1]);
    section.tagName = "SECTION";
    return { section, offset: match.index };
  });
  const panels = [...html.matchAll(/<(div|details)\b[^>]*id="([^"]+)"[^>]*data-admin-view/g)].map(match => {
    const panel = elements.get(match[2]);
    panel.tagName = match[1].toUpperCase();
    panel.adminView = true;
    panel.parentElement = sections.filter(entry => entry.offset < match.index).at(-1).section;
    return panel;
  });
  elements.get("token-result").parentElement = elements.get("admin-token-management");
  const document = {
    body: element("body"),
    getElementById: id => elements.get(id) || null,
    createElement: () => element("", "A"),
    addEventListener() {},
    querySelectorAll(selector) {
      if (selector === "main > section") return sections.map(entry => entry.section);
      if (selector === "[data-admin-view]") return panels;
      return [];
    }
  };
  const root = {
    document, location: { hash: "" }, scrollY: 0,
    matchMedia: () => ({ matches: false, addEventListener() {} }),
    history: { pushState(_, __, hash) { root.location.hash = hash; } },
    addEventListener() {}, requestAnimationFrame(callback) { callback(); }, scrollTo() {}
  };
  vm.runInNewContext(fs.readFileSync(path.join(__dirname, "../src/dashboard/navigation.js"), "utf8"), { window: root });
  root.VLiteNavigation.reveal("admin-backups");
  assert.equal(elements.get("admin-backups").hidden, false);
  assert.equal(elements.get("admin-backups").open, true);
  assert.equal(elements.get("admin-token-management").hidden, true);
  root.VLiteNavigation.reveal("token-result");
  assert.equal(elements.get("admin-token-management").hidden, false);
  assert.equal(elements.get("admin-token-management").open, true);
  root.VLiteNavigation.reveal("admin-backup-schedule");
  assert.equal(elements.get("admin-backups").hidden, true);
  assert.equal(elements.get("admin-backup-schedule").open, true);
  root.VLiteNavigation.reveal("operations");
  for (const panel of panels) assert.equal(panel.hidden, false);
});

test("admin integration shares transport and exposes all module controls", () => {
  const app = fs.readFileSync(path.join(__dirname, "../src/dashboard/app.js"), "utf8");
  const server = fs.readFileSync(path.join(__dirname, "../src/server.rs"), "utf8");
  assert.ok(html.indexOf('src="/dashboard/admin-tools.js" defer') < html.indexOf('src="/dashboard/app.js" defer'));
  assert.match(app, /VLiteTools\.init\(toolsContext\)/);
  assert.match(app, /VLiteAdminTools\.init\(toolsContext\)/);
  assert.match(app, /adminTools\?\.capabilities\(capabilities\)/);
  assert.match(app, /adminTools\?\.disconnect\(\)/);
  assert.match(app, /adminTools\?\.gate\?\.\(\)/);
  assert.match(server, /include_str!\("dashboard\/admin-tools\.js"\)/);
  assert.match(server, /\| "\/dashboard\/admin-tools\.js"/);
  for (const id of [
    "tenant-list", "tenant-name", "tenant-max-vectors", "tenant-create", "tenant-id", "tenant-delete", "tenant-result",
    "tenant-usage", "tenant-new-name", "tenant-limit-mode", "tenant-new-limit", "tenant-save-settings", "tenant-suspend", "tenant-resume",
    "identity-badge", "identity-name", "identity-detail",
    "tls-reload", "tls-result", "replication-after", "replication-max-bytes", "replication-export", "replication-download", "replication-result",
    "cred-tenant", "cred-rpm", "cred-max-vectors", "cred-max-vectors-per-collection"
  ]) assert.match(html, new RegExp(`id="${id}"`));
  for (const id of ["tenant-list", "tenant-create", "tenant-delete"]) {
    assert.match(html, new RegExp(`id="${id}"[^>]*data-admin[^>]*data-feature="tenants"`));
  }
  for (const id of ["tenant-usage", "tenant-save-settings", "tenant-suspend", "tenant-resume"]) {
    assert.match(html, new RegExp(`id="${id}"[^>]*data-admin[^>]*data-feature="tenants tenant_lifecycle"`));
  }
  assert.doesNotMatch(html, /no tenant rename or quota-update endpoint/);
  assert.match(html, /id="tls-reload"[^>]*data-admin[^>]*data-feature="tls_reload"/);
  assert.match(html, /Requests per second \(1\.\.1000000/);
  assert.match(html, /data-admin-availability="scoped_credentials"/);
});

test("collection write and search variants keep their parent tab", () => {
  assert.equal(resolveRoute("write-json").tab, "editor");
  for (const [id, section] of [["collection-search", "search"], ["collection-retrieval", "retrieval"], ["collection-explore", "explore"]]) {
    assert.deepEqual(resolveRoute(id), { id, section, workspace: "collections", tab: "collection-search" });
  }
});

test("unknown and malformed targets fall back safely without throwing", () => {
  for (const target of ["", null, undefined, "#not-a-view", "#%zz", "__proto__", "constructor"]) {
    assert.equal(resolveRoute(target).section, "database");
    assert.equal(resolveRoute(target).id, "database");
  }
  assert.equal(resolveRoute("#%62rowse").section, "browse");
  assert.equal(normalizeTarget("#collection-search"), "collection-search");
});

test("tab keyboard navigation wraps and supports Home and End", () => {
  assert.equal(nextTabIndex(0, "ArrowLeft", 5), 4);
  assert.equal(nextTabIndex(4, "ArrowRight", 5), 0);
  assert.equal(nextTabIndex(1, "ArrowDown", 5), 2);
  assert.equal(nextTabIndex(1, "ArrowUp", 5), 0);
  assert.equal(nextTabIndex(3, "Home", 5), 0);
  assert.equal(nextTabIndex(1, "End", 5), 4);
  assert.equal(nextTabIndex(1, "Enter", 5), -1);
  assert.equal(nextTabIndex(0, "Home", 0), -1);
});

test("shell keeps unique IDs and loads navigation before application code", () => {
  const ids = Array.from(html.matchAll(/\bid="([^"]+)"/g), match => match[1]);
  assert.equal(new Set(ids).size, ids.length);
  assert.ok(html.indexOf('src="/dashboard/navigation.js" defer') < html.indexOf('src="/dashboard/app.js" defer'));
  assert.equal((html.match(/data-workspace="/g) || []).length, 6);
  assert.match(html, /id="navigation-toggle"[^>]*aria-expanded="false"/);
});

test("record inspector tab contracts preserve legacy outputs", () => {
  for (const name of ["payload", "metadata", "vector", "raw"]) {
    assert.match(html, new RegExp(`data-record-tab="${name}"`));
    assert.match(html, new RegExp(`id="record-inspector-${name}"[^>]*role="tabpanel"`));
  }
  assert.match(html, /id="browse-preview"/);
  assert.match(html, /id="record" /);
  assert.match(html, /id="record-inspector-raw-content"/);
});
