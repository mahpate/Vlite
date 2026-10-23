const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const { resolveRoute, nextTabIndex, normalizeTarget } = require("../src/dashboard/navigation.js");

const html = fs.readFileSync(path.join(__dirname, "../src/dashboard/index.html"), "utf8");

test("every legacy section hash selects its containing workspace", () => {
  const expected = {
    database: "overview", collections: "collections", browse: "collections", editor: "collections",
    "write-json": "collections", index: "collections", search: "search", retrieval: "search",
    explore: "search", operations: "administration", credentials: "administration", webhooks: "administration",
    observability: "observability", diagnostics: "observability", "change-feed": "observability",
    requests: "developer", "data-tools": "developer", "resumable-import": "developer"
  };
  const sectionIds = Array.from(html.matchAll(/<section\b[^>]*\bid="([^"]+)"/g), match => match[1]);
  assert.deepEqual(sectionIds.slice().sort(), Object.keys(expected).sort());
  for (const [id, workspace] of Object.entries(expected)) {
    assert.deepEqual(resolveRoute(`#${id}`), { id, workspace, section: id, tab: id === "write-json" ? "editor" : id });
  }
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
