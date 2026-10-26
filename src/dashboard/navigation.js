(function (root) {
  "use strict";

  // Workspaces group related views (tabs). Some views split into tools (subtabs).
  const workspaces = {
    overview: { title: "Overview", description: "Your database at a glance: size, health and shortcuts.", tabs: [] },
    collections: {
      title: "Collections", description: "Records, imports and indexes in the active collection.",
      tabs: [["browse", "Records"], ["editor", "Add & edit"], ["data-tools", "Import & export"], ["index", "Index"], ["collections", "Manage"]]
    },
    search: {
      title: "Search", description: "Find similar records with your own query vectors. VLite does not create embeddings.",
      tabs: [["search", "Vector search"], ["retrieval", "Advanced retrieval"], ["explore", "Analyze & compare"]]
    },
    graph: { title: "Graph", description: "Read-only explorer for this file's property graphs, bounded by server limits.", tabs: [] },
    observability: {
      title: "Observability", description: "Live server activity, diagnostics and the change feed.",
      tabs: [["observability", "Live monitor"], ["diagnostics", "Diagnostics"], ["change-feed", "Change feed"]]
    },
    administration: {
      title: "Administration", description: "Server health, people and access, backups and settings. Changes ask for confirmation.",
      tabs: [["operations", "Health"], ["users", "Users & access"], ["admin-backups", "Backup & recovery"], ["admin-auto-maintenance", "Server settings"]]
    },
    developer: { title: "Developer Console", description: "Inspect the requests this dashboard sends and the responses it gets.", tabs: [] }
  };
  // Tools inside a view, keyed by the view's tab. The first tool is the view itself.
  const viewTools = {
    editor: [["editor", "One record"], ["write-json", "Bulk JSON"]],
    "data-tools": [["data-tools", "Quick import & export"], ["resumable-import", "Large file import"]],
    users: [["users", "Users"], ["credentials", "API keys"], ["tenants", "Tenants"], ["admin-token-management", "Admin tokens"], ["admin-persistent-audit", "Audit log"]],
    "admin-backups": [["admin-backups", "Backups"], ["admin-backup-schedule", "Schedule"], ["admin-pitr", "Point-in-time restore"], ["replication", "Replication"]],
    "admin-auto-maintenance": [["admin-auto-maintenance", "Automation"], ["webhooks", "Webhooks"], ["transport-security", "TLS certificates"]]
  };
  // Links from earlier dashboard versions keep working.
  const aliases = {
    "collection-search": "search", "collection-retrieval": "retrieval", "collection-explore": "explore",
    "admin-health": "operations", "admin-credential-form": "credentials", "admin-webhooks": "webhooks"
  };
  // Each route is [workspace, section, tab]; every section has exactly one route.
  const routes = { database: ["overview", "database", "database"], graph: ["graph", "graph", "graph"], requests: ["developer", "requests", "requests"] };
  for (const [workspace, { tabs }] of Object.entries(workspaces)) {
    for (const [tab] of tabs) {
      for (const [section] of viewTools[tab] || [[tab]]) routes[section] = [workspace, section, tab];
    }
  }
  const own = (object, key) => Object.prototype.hasOwnProperty.call(object, key);
  const hasRoute = id => own(routes, id) || own(aliases, id);

  function normalizeTarget(target) {
    try { return decodeURIComponent(String(target || "").replace(/^#/, "")); }
    catch (_) { return ""; }
  }

  function resolveRoute(target) {
    const requested = normalizeTarget(target);
    const id = own(aliases, requested) ? aliases[requested] : own(routes, requested) ? requested : "database";
    const [workspace, section, tab] = routes[id];
    return { id, workspace, section, tab };
  }

  function nextTabIndex(index, key, count) {
    if (!count) return -1;
    if (key === "Home") return 0;
    if (key === "End") return count - 1;
    if (key === "ArrowRight" || key === "ArrowDown") return (index + 1) % count;
    if (key === "ArrowLeft" || key === "ArrowUp") return (index + count - 1) % count;
    return -1;
  }

  const api = { resolveRoute, nextTabIndex, normalizeTarget };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  if (!root.document) return;

  const document = root.document;
  const sections = Array.from(document.querySelectorAll("main > section"));
  const primary = Array.from(document.querySelectorAll("[data-workspace]"));
  const tabs = document.getElementById("workspace-tabs");
  const subtabs = document.getElementById("workspace-subtabs");
  const sidebar = document.getElementById("dashboard-sidebar");
  const toggle = document.getElementById("navigation-toggle");
  const backdrop = document.getElementById("navigation-backdrop");
  const userMenu = document.getElementById("user-menu");
  const mobile = root.matchMedia("(max-width: 760px)");
  const scrollPositions = new Map();
  let current = null;
  let drawerOpen = false;
  let renderVersion = 0;
  let renderedHash = null;

  function openDetails(target) {
    for (let parent = target; parent; parent = parent.parentElement) {
      if (parent.tagName === "DETAILS") parent.open = true;
    }
  }

  function inspectorTab(name) {
    document.querySelectorAll("[data-record-tab]").forEach(button => {
      const selected = button.dataset.recordTab === name;
      button.setAttribute("aria-selected", String(selected));
      button.tabIndex = selected ? 0 : -1;
      const panel = document.getElementById(button.getAttribute("aria-controls"));
      if (panel) panel.hidden = !selected;
    });
  }

  function setDrawer(open, restoreFocus) {
    drawerOpen = Boolean(open && mobile.matches);
    sidebar.classList.toggle("is-open", drawerOpen);
    sidebar.inert = mobile.matches && !drawerOpen;
    toggle.setAttribute("aria-expanded", String(drawerOpen));
    backdrop.hidden = !drawerOpen;
    document.body.classList.toggle("navigation-open", drawerOpen);
    if (drawerOpen) {
      sidebar.setAttribute("role", "dialog");
      sidebar.setAttribute("aria-modal", "true");
      (sidebar.querySelector('[aria-current="page"]') || primary[0]).focus();
    } else {
      sidebar.removeAttribute("role");
      sidebar.removeAttribute("aria-modal");
      if (restoreFocus) toggle.focus();
    }
  }

  function closeUserMenu(restoreFocus) {
    if (!userMenu || !userMenu.open) return;
    userMenu.open = false;
    if (restoreFocus) userMenu.querySelector("summary").focus();
  }

  function renderTabs(container, entries, selected, label) {
    container.replaceChildren();
    container.hidden = !entries.length;
    container.setAttribute("aria-label", label);
    entries.forEach(([id, title]) => {
      const link = document.createElement("a");
      const route = resolveRoute(id);
      link.id = `${container.id}-${id}`;
      link.href = `#${id}`;
      link.textContent = title;
      link.setAttribute("role", "tab");
      link.setAttribute("aria-controls", route.section);
      link.setAttribute("aria-selected", String(id === selected));
      link.tabIndex = id === selected ? 0 : -1;
      container.append(link);
    });
  }

  function render(targetId, options = {}) {
    const id = normalizeTarget(targetId);
    const target = document.getElementById(id);
    const containing = target && target.closest("main > section");
    const route = resolveRoute(hasRoute(id) ? id : containing && containing.id);
    if (current && !options.skipSave) scrollPositions.set(current.id, root.scrollY);
    current = route;
    renderedHash = root.location.hash;
    const workspace = workspaces[route.workspace];
    const version = ++renderVersion;
    sections.forEach(section => { section.hidden = section.id !== route.section; });
    const inspector = document.getElementById("record-inspector");
    const inspectorSlot = document.getElementById(route.section === "search" ? "search-inspector-slot" : "browse-inspector-slot");
    if (inspector && inspectorSlot && inspector.parentElement !== inspectorSlot) inspectorSlot.append(inspector);
    primary.forEach(link => {
      if (link.dataset.workspace === route.workspace) link.setAttribute("aria-current", "page");
      else link.removeAttribute("aria-current");
    });
    document.getElementById("workspace-title").textContent = workspace.title;
    document.getElementById("workspace-description").textContent = workspace.description;
    renderTabs(tabs, workspace.tabs, route.tab, `${workspace.title} views`);
    const tools = own(viewTools, route.tab) ? viewTools[route.tab] : [];
    const view = workspace.tabs.find(([tab]) => tab === route.tab);
    renderTabs(subtabs, tools, route.section, `${view ? view[1] : workspace.title} tools`);
    const visible = document.getElementById(route.section);
    visible.setAttribute("role", "tabpanel");
    const selectedLink = (tools.length ? subtabs : tabs).querySelector('[aria-selected="true"]');
    if (selectedLink) visible.setAttribute("aria-labelledby", selectedLink.id);
    else {
      visible.removeAttribute("role");
      visible.removeAttribute("aria-labelledby");
    }
    openDetails(target);
    const inspectorPanel = target && target.closest("#record-inspector > [role='tabpanel']");
    if (inspectorPanel) inspectorTab(inspectorPanel.id.replace("record-inspector-", ""));
    if (options.focusTab) {
      const focusGroup = options.focusGroup && document.getElementById(options.focusGroup);
      const focusLink = focusGroup ? focusGroup.querySelector('[aria-selected="true"]') : selectedLink;
      if (focusLink) focusLink.focus({ preventScroll: true });
    }
    root.requestAnimationFrame(() => {
      if (version !== renderVersion) return;
      if (target && target !== visible && options.scrollTarget) target.scrollIntoView({ block: "nearest" });
      else if (!options.keepScroll) root.scrollTo(0, scrollPositions.get(route.id) || 0);
    });
    return visible;
  }

  function navigate(id, options = {}) {
    if (current) scrollPositions.set(current.id, root.scrollY);
    const hash = `#${normalizeTarget(id)}`;
    if (root.location.hash !== hash) root.history.pushState(null, "", hash);
    return render(id, { ...options, skipSave: true });
  }

  // Shows the view holding a section or control, opening any collapsed details around it.
  api.reveal = function (targetId) {
    const id = normalizeTarget(targetId);
    const target = document.getElementById(id);
    if (!target && !hasRoute(id)) return null;
    const containing = target && target.closest("main > section");
    if (containing && current && containing.id === current.section) {
      openDetails(target);
      const panel = target.closest("#record-inspector > [role='tabpanel']");
      if (panel) inspectorTab(panel.id.replace("record-inspector-", ""));
      return containing;
    }
    if (target && !containing && !hasRoute(id)) {
      openDetails(target);
      return target;
    }
    return navigate(id, { scrollTarget: false, keepScroll: true });
  };
  root.VLiteNavigation = api;
  if ("scrollRestoration" in root.history) root.history.scrollRestoration = "manual";

  document.addEventListener("click", event => {
    // The account menu closes on any click outside it and after choosing one of its actions.
    if (userMenu && userMenu.open && (!userMenu.contains(event.target) || event.target.closest(".user-menu-panel button"))) closeUserMenu(false);
    const inspector = event.target.closest("[data-record-tab]");
    if (inspector) { inspectorTab(inspector.dataset.recordTab); return; }
    const link = event.target.closest('a[href^="#"]');
    if (!link || event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    const id = normalizeTarget(link.getAttribute("href"));
    const target = document.getElementById(id);
    if (id === "workspace-content" && target) {
      // The skip link moves focus without changing the view.
      event.preventDefault();
      target.tabIndex = -1;
      target.focus();
      return;
    }
    if (!hasRoute(id) && !(target && target.closest("main > section"))) return;
    event.preventDefault();
    const wasDrawerOpen = drawerOpen;
    const visible = navigate(id, { focusTab: link.getAttribute("role") === "tab", focusGroup: link.parentElement.id, scrollTarget: target && !sections.includes(target) });
    setDrawer(false, false);
    if (wasDrawerOpen) {
      const heading = visible.querySelector("h2");
      heading.tabIndex = -1;
      heading.focus({ preventScroll: true });
    }
  });
  document.addEventListener("keydown", event => {
    if (event.key === "Escape" && userMenu && userMenu.open && userMenu.contains(event.target)) { event.preventDefault(); closeUserMenu(true); return; }
    if (event.key === "Escape" && drawerOpen) { event.preventDefault(); setDrawer(false, true); return; }
    if (drawerOpen && event.key === "Tab") {
      const focusable = Array.from(sidebar.querySelectorAll('a[href],button:not([disabled]),select:not([disabled]),input:not([disabled])')).filter(element => element.getClientRects().length);
      const first = focusable[0], last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
    }
    const group = event.target.closest('[role="tablist"], .sidebar nav');
    if (!group) return;
    const items = Array.from(group.querySelectorAll('[role="tab"], a[data-workspace]'));
    const index = items.indexOf(event.target);
    if (index < 0) return;
    const next = nextTabIndex(index, event.key, items.length);
    if (next < 0) return;
    event.preventDefault();
    const item = items[next];
    if (item.dataset.recordTab) { inspectorTab(item.dataset.recordTab); item.focus(); }
    else if (item.getAttribute("role") === "tab") navigate(item.hash, { focusTab: true, focusGroup: group.id });
    else item.focus();
  });
  if (userMenu) {
    // Keyboard users leave the menu by tabbing out of it.
    userMenu.addEventListener("focusout", event => {
      if (event.relatedTarget && !userMenu.contains(event.relatedTarget)) closeUserMenu(false);
    });
  }
  toggle.addEventListener("click", () => setDrawer(!drawerOpen, false));
  document.getElementById("navigation-close").addEventListener("click", () => setDrawer(false, true));
  backdrop.addEventListener("click", () => setDrawer(false, true));
  mobile.addEventListener("change", () => {
    const focusWasInSidebar = sidebar.contains(document.activeElement);
    setDrawer(false, mobile.matches && focusWasInSidebar);
  });
  function syncLocation() {
    if (renderedHash !== root.location.hash) render(root.location.hash, { scrollTarget: true });
  }
  root.addEventListener("hashchange", syncLocation);
  root.addEventListener("popstate", syncLocation);
  render(root.location.hash, { scrollTarget: true });
  setDrawer(false, false);
})(typeof window !== "undefined" ? window : globalThis);
