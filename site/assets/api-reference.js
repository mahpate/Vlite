(() => {
  const main = document.querySelector(".api-ref");
  const browser = document.getElementById("api-browser");
  if (!main || !browser) return;
  const families = {
    rest: ["REST", "HTTP routes with explicit methods and credential scopes.", "docs/API.html"],
    node: ["Node.js", "Embedded Promise-based methods. IDs and revisions are decimal strings.", "docs/REFERENCE.html"],
    rust: ["Rust", "Native types, methods and functions in the vlite crate.", "docs/REFERENCE.html"],
    c: ["C ABI", "Native functions and UTF-8 JSON requests through vlite_execute.", "docs/REFERENCE.html"],
    py: ["Python", "Embedded synchronous methods. IDs and revisions are decimal strings.", "docs/REFERENCE.html"],
    grpc: ["gRPC", "Methods on vlite.v1.VLite. Graph operations remain REST-only.", "docs/GRPC.html"],
    mcp: ["MCP", "Agent tools over stdio. Mutations require the --writable option.", "docs/MCP.html"],
    wasm: ["WebAssembly", "In-memory databases. Persist byte images explicitly.", "docs/REFERENCE.html"],
  };
  const make = (tag, text, attributes = {}) => {
    const node = document.createElement(tag);
    if (text) node.textContent = text;
    for (const [name, value] of Object.entries(attributes)) node.setAttribute(name, value);
    return node;
  };
  const copyContent = source => {
    const fragment = document.createDocumentFragment();
    for (const node of source.childNodes) fragment.append(node.cloneNode(true));
    fragment.querySelectorAll("[id], [data-api]").forEach(node => {
      node.removeAttribute("id");
      node.removeAttribute("data-api");
    });
    return fragment;
  };
  const entries = [];
  let section, group;
  for (const element of main.querySelectorAll("h2, h3, tr")) {
    if (element.tagName === "H2") { section = group = element; }
    if (element.tagName === "H3") group = element;
    if (element.tagName !== "TR" || !section) continue;
    for (const signature of element.querySelectorAll("[data-api]")) {
      const key = signature.dataset.api;
      const prefix = key.split(":")[0];
      const family = prefix === "c-fn" ? "c" : prefix;
      if (!families[family]) continue;
      const title = family === "rest" ? key.slice(5).split(" ")[0] + " " + signature.textContent.trim() : key.slice(key.indexOf(":") + 1);
      const category = family === "rest" && title.startsWith("GET /dashboard") ? "Dashboard assets" : group.textContent.trim();
      const description = element.cells[1];
      entries.push({
        key, family, title, category, signature, description,
        access: element.cells[2], group, section,
        search: `${title} ${signature.textContent} ${category} ${description.textContent}`.toLowerCase(),
      });
    }
  }
  const byKey = new Map(entries.map(entry => [entry.key, entry]));
  // Put API operations before supporting static assets, without hiding either.
  entries.sort((a, b) => Number(a.category === "Dashboard assets") - Number(b.category === "Dashboard assets"));
  const full = make("details", "", { class: "api-full-reference" });
  full.append(make("summary", "Full reference, quick starts and shared types"));
  let next = document.getElementById("embedded");
  main.insertBefore(full, next);
  while (next) {
    const following = next.nextElementSibling;
    full.append(next);
    next = following;
  }
  const search = document.getElementById("api-search");
  const category = document.getElementById("api-category");
  const results = document.getElementById("api-results");
  const detail = document.getElementById("api-detail");
  const count = document.getElementById("api-count");
  const revealSelection = () => {
    const active = results.querySelector('[aria-current="true"]');
    if (active) results.scrollTop = Math.max(0, active.offsetTop - results.clientHeight / 2 + active.offsetHeight / 2);
  };
  new ResizeObserver(revealSelection).observe(results);
  let family = "rest", selected = null;
  const familyButtons = new Map();
  for (const [key, [label]] of Object.entries(families)) {
    const button = make("button", label, { type: "button", "aria-pressed": "false", "data-family": key });
    button.addEventListener("click", () => navigate(`browse=${key}`));
    browser.querySelector(".api-families").append(button);
    familyButtons.set(key, button);
  }
  const showDetail = entry => {
    selected = entry;
    detail.replaceChildren();
    delete detail.dataset.key;
    results.querySelectorAll("a").forEach(link => {
      if (link.dataset.key === entry?.key) link.setAttribute("aria-current", "true");
      else link.removeAttribute("aria-current");
    });
    if (!entry) {
      detail.append(make("h3", "No matching calls", { id: "api-detail-title" }),
        make("p", "Try a shorter term, choose All areas, or clear the filters."));
      return;
    }
    detail.dataset.key = entry.key;
    detail.append(make("p", `${families[family][0]} / ${entry.category}`, { class: "kicker" }),
      make("h3", entry.title, { id: "api-detail-title" }));
    const actions = make("div", "", { class: "api-detail-actions" });
    const permalink = make("a", "Link to this call", { href: `#api=${encodeURIComponent(entry.key)}` });
    const copy = make("button", "Copy signature", { type: "button" });
    copy.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText((family === "rest" ? entry.title.split(" ")[0] + " " : "") + entry.signature.textContent.trim());
        copy.textContent = "Copied";
      } catch {
        copy.textContent = "Select the signature to copy";
      }
    });
    actions.append(copy, permalink);
    const signature = make("pre", "", { class: "code api-signature", tabindex: "0" });
    signature.append(make("code", (family === "rest" ? entry.title.split(" ")[0] + " " : "") + entry.signature.textContent.trim()));
    const behavior = make("div", "", { class: "api-behavior" });
    behavior.append(copyContent(entry.description));
    detail.append(actions, make("h4", "Signature"), signature, make("h4", "Behavior"), behavior,
      make("h4", "Access and execution"));
    const access = make("p");
    if (entry.access) access.append(copyContent(entry.access));
    else access.textContent = "Runs inside your application; no HTTP credential is used. File access and the handle's read/write mode still apply.";
    detail.append(access);
    const context = entry.section.nextElementSibling;
    if (context?.tagName === "P") {
      const notes = make("details", "", { class: "api-usage" });
      const text = make("p");
      text.append(copyContent(context));
      notes.append(make("summary", `Using ${families[family][0]}`), text);
      detail.append(notes);
    }
    const links = make("p", "", { class: "api-detail-actions" });
    const guide = entry.category === "Property graph" ? "docs/GRAPH.html" : families[family][2];
    links.append(make("a", "Full section", { href: `#${entry.group.id}` }),
      make("a", "Request formats and examples", { href: guide }));
    detail.append(links);
  };
  const render = preferred => {
    const words = search.value.toLowerCase().trim().split(/\s+/).filter(Boolean);
    const matching = entries.filter(entry => entry.family === family &&
      (!category.value || entry.category === category.value) && words.every(word => entry.search.includes(word)));
    count.textContent = `${matching.length} of ${entries.filter(entry => entry.family === family).length} ${families[family][0]} entries`;
    const fragment = document.createDocumentFragment();
    for (const entry of matching) {
      const link = make("a", "", { href: `#api=${encodeURIComponent(entry.key)}`, "data-key": entry.key });
      link.append(make("code", entry.title), make("span", entry.category));
      link.addEventListener("click", event => {
        event.preventDefault();
        navigate(`api=${encodeURIComponent(entry.key)}`);
        detail.focus({ preventScroll: true });
        if (matchMedia("(max-width: 700px)").matches) detail.scrollIntoView({ block: "start" });
      });
      fragment.append(link);
    }
    results.replaceChildren(fragment);
    showDetail(matching.find(entry => entry === preferred) || matching[0]);
    revealSelection();
  };
  const chooseFamily = value => {
    family = value;
    search.value = "";
    category.replaceChildren(make("option", "All areas", { value: "" }));
    for (const area of new Set(entries.filter(entry => entry.family === family).map(entry => entry.category))) {
      category.append(make("option", area, { value: area }));
    }
    familyButtons.forEach((button, key) => button.setAttribute("aria-pressed", String(key === family)));
    document.querySelectorAll(".api-sidebar [data-api-family]").forEach(link => {
      if (link.dataset.apiFamily === family) link.setAttribute("aria-current", "true");
      else link.removeAttribute("aria-current");
    });
    document.getElementById("api-family-description").textContent = families[family][1];
    render();
  };
  const followHash = () => {
    const hash = location.hash.slice(1);
    if (hash.startsWith("api=")) {
      let key;
      try { key = decodeURIComponent(hash.slice(4)); } catch { key = null; }
      const entry = byKey.get(key);
      if (!entry) {
        count.textContent = "That API link is not available. Choose a family or search for a call.";
        return;
      }
      if (family !== entry.family || search.value || category.value) chooseFamily(entry.family);
      render(entry);
    } else if (hash.startsWith("browse=") && families[hash.slice(7)]) {
      chooseFamily(hash.slice(7));
    } else {
      const target = document.getElementById(hash);
      if (target && full.contains(target)) {
        full.open = true;
        target.scrollIntoView();
      }
    }
  };
  const navigate = hash => {
    if (location.hash.slice(1) !== hash) history.pushState(null, "", `#${hash}`);
    followHash();
  };
  const sidebar = document.querySelector(".docs-sidebar");
  sidebar.classList.add("api-sidebar");
  const menu = make("div", "", { class: "docs-group" });
  menu.append(make("h4", "API families"));
  for (const [key, [label]] of Object.entries(families)) {
    menu.append(make("a", label, { href: `#browse=${key}`, "data-api-family": key }));
  }
  const reference = make("div", "", { class: "docs-group" });
  reference.append(make("h4", "Guides and conventions"));
  for (const [id, label] of [["embedded", "Quick start"], ["conventions", "Conventions"], ["types", "Shared types"]]) {
    reference.append(make("a", label, { href: `#${id}` }));
  }
  sidebar.replaceChildren(menu, reference);
  for (const card of document.querySelectorAll("[data-api-family]")) {
    card.addEventListener("click", event => {
      event.preventDefault();
      const value = card.dataset.apiFamily;
      navigate(`browse=${value}`);
      browser.scrollIntoView();
      familyButtons.get(value).focus({ preventScroll: true });
    });
  }
  search.addEventListener("input", () => render(selected));
  category.addEventListener("change", () => render(selected));
  document.getElementById("api-clear").addEventListener("click", () => {
    chooseFamily(family);
    search.focus();
  });
  // Repeated fragment clicks must reopen the printable reference too.
  document.addEventListener("click", event => {
    const link = event.target.closest("a[href^='#']");
    if (!link) return;
    const target = document.getElementById(link.getAttribute("href").slice(1));
    if (target && full.contains(target)) full.open = true;
  });
  window.addEventListener("hashchange", followHash);
  window.addEventListener("popstate", followHash);
  let openBeforePrint = false;
  window.addEventListener("beforeprint", () => { openBeforePrint = full.open; full.open = true; });
  window.addEventListener("afterprint", () => { full.open = openBeforePrint; });
  chooseFamily(family);
  browser.hidden = false;
  followHash();
})();
