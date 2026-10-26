const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { resolveRoute, nextTabIndex, normalizeTarget } = require("../src/dashboard/navigation.js");

const html = fs.readFileSync(path.join(__dirname, "../src/dashboard/index.html"), "utf8");
const sectionMatches = [...html.matchAll(/<section\b[^>]*\bid="([^"]+)"/g)];

// Section -> [workspace, top-level view tab that selects it].
const layout = {
  database: ["overview", "database"],
  browse: ["collections", "browse"], editor: ["collections", "editor"], "write-json": ["collections", "editor"],
  "data-tools": ["collections", "data-tools"], "resumable-import": ["collections", "data-tools"],
  index: ["collections", "index"], collections: ["collections", "collections"],
  search: ["search", "search"], retrieval: ["search", "retrieval"], explore: ["search", "explore"],
  graph: ["graph", "graph"],
  observability: ["observability", "observability"], diagnostics: ["observability", "diagnostics"],
  "change-feed": ["observability", "change-feed"],
  operations: ["administration", "operations"],
  users: ["administration", "users"], credentials: ["administration", "users"], tenants: ["administration", "users"],
  "admin-token-management": ["administration", "users"], "admin-persistent-audit": ["administration", "users"],
  "admin-backups": ["administration", "admin-backups"], "admin-backup-schedule": ["administration", "admin-backups"],
  "admin-pitr": ["administration", "admin-backups"], replication: ["administration", "admin-backups"],
  "admin-auto-maintenance": ["administration", "admin-auto-maintenance"],
  webhooks: ["administration", "admin-auto-maintenance"], "transport-security": ["administration", "admin-auto-maintenance"],
  requests: ["developer", "requests"]
};

test("every section hash selects its workspace and view", () => {
  assert.deepEqual(sectionMatches.map(match => match[1]).sort(), Object.keys(layout).sort());
  for (const [id, [workspace, tab]] of Object.entries(layout)) {
    assert.deepEqual(resolveRoute(`#${id}`), { id, workspace, section: id, tab });
  }
});

test("links from earlier dashboard versions open the view that holds them now", () => {
  for (const [alias, id] of [
    ["collection-search", "search"], ["collection-retrieval", "retrieval"], ["collection-explore", "explore"],
    ["admin-health", "operations"], ["admin-credential-form", "credentials"], ["admin-webhooks", "webhooks"]
  ]) assert.deepEqual(resolveRoute(`#${alias}`), resolveRoute(id));
  for (const [anchor, section] of [["admin-health", "operations"], ["admin-credential-form", "credentials"], ["admin-webhooks", "webhooks"]]) {
    const offset = html.indexOf(`id="${anchor}"`);
    assert.ok(offset > 0, anchor);
    assert.equal(sectionMatches.filter(match => match.index < offset).at(-1)[1], section, anchor);
  }
  assert.doesNotMatch(html, /data-admin-view/);
});

function dashboardDom() {
  const elements = new Map();
  function element(id, tagName = "DIV") {
    const attributes = new Map();
    const item = {
      id, tagName, hidden: false, open: false, children: [], dataset: {}, textContent: "",
      parentElement: null, classList: { toggle() {} },
      setAttribute(name, value) { attributes.set(name, String(value)); },
      getAttribute(name) { return attributes.has(name) ? attributes.get(name) : null; },
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
        }
        return null;
      }
    };
    if (id) elements.set(id, item);
    return item;
  }
  // Navigation only walks sections and details, so nest those as the markup does.
  const containers = [];
  for (const tag of ["section", "details"]) {
    const open = [];
    for (const match of html.matchAll(new RegExp(`<(/?)${tag}\\b[^>]*>`, "g"))) {
      if (match[1]) containers.push({ ...open.pop(), end: match.index });
      else open.push({ start: match.index, node: element((match[0].match(/\sid="([^"]+)"/) || [])[1], tag.toUpperCase()) });
    }
    assert.equal(open.length, 0, `unbalanced <${tag}> markup`);
  }
  const enclosing = offset => containers.filter(item => item.start < offset && offset < item.end).sort((a, b) => b.start - a.start)[0];
  for (const item of containers) item.node.parentElement = enclosing(item.start)?.node || null;
  for (const match of html.matchAll(/<([a-zA-Z][\w-]*)\s(?:[^>]*\s)?id="([^"]+)"/g)) {
    if (elements.has(match[2])) continue;
    element(match[2], match[1].toUpperCase()).parentElement = enclosing(match.index)?.node || null;
  }
  const sections = containers.filter(item => item.node.tagName === "SECTION").sort((a, b) => a.start - b.start).map(item => item.node);
  const navigation = Array.from(html.matchAll(/data-workspace="([^"]+)"/g), match => {
    const link = element("", "A");
    link.dataset.workspace = match[1];
    return link;
  });
  const document = {
    body: element("", "BODY"),
    getElementById: id => elements.get(id) || null,
    createElement: tag => element("", tag.toUpperCase()),
    addEventListener() {},
    querySelectorAll(selector) {
      if (selector === "main > section") return sections;
      if (selector === "[data-workspace]") return navigation;
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
  return { root, elements, sections, navigation };
}

test("revealing a view or control shows its section under grouped tabs", () => {
  const { root, elements, sections, navigation } = dashboardDom();
  const reveal = root.VLiteNavigation.reveal;
  const visible = () => sections.filter(section => !section.hidden).map(section => section.id);
  const labels = id => elements.get(id).children.map(link => link.textContent);
  const selected = id => labels(id).filter((_, index) => elements.get(id).children[index].getAttribute("aria-selected") === "true");
  const current = () => navigation.filter(link => link.getAttribute("aria-current") === "page").map(link => link.dataset.workspace);

  assert.deepEqual(visible(), ["database"]);
  assert.deepEqual(current(), ["overview"]);
  assert.equal(elements.get("workspace-tabs").hidden, true);
  assert.equal(elements.get("workspace-subtabs").hidden, true);

  reveal("credentials");
  assert.deepEqual(visible(), ["credentials"]);
  assert.deepEqual(current(), ["administration"]);
  assert.equal(root.location.hash, "#credentials");
  assert.equal(elements.get("workspace-title").textContent, "Administration");
  assert.match(elements.get("workspace-description").textContent, /people and access/);
  assert.deepEqual(labels("workspace-tabs"), ["Health", "Users & access", "Backup & recovery", "Server settings"]);
  assert.deepEqual(selected("workspace-tabs"), ["Users & access"]);
  assert.equal(elements.get("workspace-tabs").getAttribute("aria-label"), "Administration views");
  assert.deepEqual(labels("workspace-subtabs"), ["Users", "API keys", "Tenants", "Admin tokens", "Audit log"]);
  assert.deepEqual(selected("workspace-subtabs"), ["API keys"]);
  assert.equal(elements.get("workspace-subtabs").getAttribute("aria-label"), "Users & access tools");
  assert.equal(elements.get("credentials").getAttribute("aria-labelledby"), "workspace-subtabs-credentials");

  reveal("token-result");
  assert.deepEqual(visible(), ["admin-token-management"]);
  assert.deepEqual(selected("workspace-subtabs"), ["Admin tokens"]);

  reveal("new-collection-name");
  assert.deepEqual(visible(), ["collections"]);
  assert.deepEqual(current(), ["collections"]);
  assert.deepEqual(labels("workspace-tabs"), ["Records", "Add & edit", "Import & export", "Index", "Manage"]);
  assert.deepEqual(selected("workspace-tabs"), ["Manage"]);
  assert.equal(elements.get("workspace-subtabs").hidden, true);
  assert.equal(elements.get("collections").getAttribute("aria-labelledby"), "workspace-tabs-collections");
  assert.equal(elements.get("new-collection-name").parentElement.open, true);

  // A control in the current view opens its details without another history entry.
  reveal("collection-drop");
  assert.equal(elements.get("collection-drop").parentElement.open, true);
  assert.equal(root.location.hash, "#new-collection-name");

  reveal("resumable-import");
  assert.deepEqual(selected("workspace-tabs"), ["Import & export"]);
  assert.deepEqual(labels("workspace-subtabs"), ["Quick import & export", "Large file import"]);
  assert.deepEqual(selected("workspace-subtabs"), ["Large file import"]);

  reveal("admin-health");
  assert.deepEqual(visible(), ["operations"]);
  assert.equal(elements.get("workspace-subtabs").hidden, true);

  reveal("graph");
  assert.deepEqual(visible(), ["graph"]);
  assert.deepEqual(current(), ["graph"]);
  assert.equal(elements.get("workspace-title").textContent, "Graph");
  assert.equal(elements.get("workspace-tabs").hidden, true);

  reveal("requests");
  assert.deepEqual(visible(), ["requests"]);
  assert.deepEqual(current(), ["developer"]);
  assert.equal(elements.get("workspace-tabs").hidden, true);
  assert.equal(elements.get("requests").getAttribute("role"), null);
  assert.equal(reveal("not-a-control"), null);
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
    "cred-tenant", "cred-rpm", "cred-max-vectors", "cred-max-vectors-per-collection",
    "sign-in", "username", "password", "change-password", "current-password", "new-password", "repeat-password",
    "change-password-cancel", "change-password-open", "session-refresh", "token-sign-in", "sign-in-setup",
    "sign-in-gate", "gate-title", "gate-intro", "gate-message", "gate-help", "gate-retry", "gate-sign-out", "user-menu",
    "user-note", "user-list", "user-id", "user-username", "user-role", "user-tenant", "user-graphs", "user-graph-access", "user-password", "user-must-change",
    "user-disabled", "user-create", "user-save", "user-reset-password", "user-delete", "user-result"
  ]) assert.match(html, new RegExp(`id="${id}"`));
  for (const id of ["user-list", "user-create", "user-save", "user-reset-password", "user-delete"]) {
    assert.match(html, new RegExp(`id="${id}"[^>]*data-admin[^>]*data-feature="accounts"`));
  }
  assert.match(html, /id="user-note" data-admin-availability="accounts"/);
  assert.match(html, /id="password" type="password" autocomplete="current-password"/);
  assert.match(html, /id="sign-in" hidden/);
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

test("unknown and malformed targets fall back safely without throwing", () => {
  for (const target of ["", null, undefined, "#not-a-view", "#%zz", "__proto__", "constructor", "hasOwnProperty"]) {
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

test("sign-in gate comes first and the grouped shell stays hidden until sign-in", () => {
  const ids = Array.from(html.matchAll(/\sid="([^"]+)"/g), match => match[1]);
  assert.equal(new Set(ids).size, ids.length);
  assert.ok(html.indexOf('src="/dashboard/navigation.js" defer') < html.indexOf('src="/dashboard/app.js" defer'));
  assert.ok(html.indexOf('id="sign-in-gate"') < html.indexOf('id="app-shell"'));
  assert.match(html, /<div id="app-shell" class="app-shell" hidden>/);
  assert.match(html, /id="token-sign-in" class="token-sign-in" open>/);
  assert.match(html, /<a class="skip-link" href="#workspace-content">/);
  assert.match(html, /<main id="workspace-content">/);
  assert.equal((html.match(/data-workspace="/g) || []).length, 7);
  assert.deepEqual(Array.from(html.matchAll(/class="nav-caption">([^<]+)</g), match => match[1]), ["Data", "Server", "Tools"]);
  assert.match(html, /id="navigation-toggle"[^>]*aria-expanded="false"/);
  assert.equal((html.match(/class="action-card" href="#/g) || []).length, 6);
  for (const match of html.matchAll(/class="action-card" href="#([^"]+)"/g)) assert.notEqual(resolveRoute(match[1]).id, "database", match[1]);
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
