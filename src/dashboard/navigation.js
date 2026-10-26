(function (root) {
  "use strict";

  const workspaces = {
    overview: { title: "Overview", description: "Your database at a glance.", tabs: [] },
    collections: {
      title: "Collections", description: "Records, queries and indexes in your active collection.",
      tabs: [["browse", "Records / Browse"], ["editor", "Write"], ["collection-search", "Search"], ["index", "Index"], ["collections", "Settings"]]
    },
    search: {
      title: "Search", description: "Test retrieval with your own vectors. No embedding generation.",
      tabs: [["search", "Playground"], ["retrieval", "Advanced retrieval"], ["explore", "Explore / compare"]]
    },
    observability: {
      title: "Observability", description: "Server activity, diagnostics and committed changes.",
      tabs: [["observability", "Server pulse"], ["diagnostics", "Diagnostics"], ["change-feed", "Change feed"]]
    },
    administration: {
      title: "Administration", description: "Operations, access and recovery. Writes require confirmation.",
      tabs: [["operations", "Health / tokens"], ["credentials", "Scoped credentials / audit"], ["tenants", "Tenants"], ["users", "Users"], ["admin-backups", "Backup / restore"], ["admin-auto-maintenance", "Automation"], ["webhooks", "Webhooks / PITR"], ["transport-security", "TLS certificates"], ["replication", "Replication export"]]
    },
    developer: {
      title: "Developer Console", description: "Inspect requests and review data workflows.",
      tabs: [["requests", "Request inspector"], ["data-tools", "Import / export"], ["resumable-import", "Resumable import"]]
    }
  };
  const collectionSearch = [["collection-search", "Playground"], ["collection-retrieval", "Advanced retrieval"], ["collection-explore", "Explore / compare"]];
  const collectionWrite = [["editor", "Record editor"], ["write-json", "JSON documents"]];
  const adminTabs = {
    operations: [["operations", "All operations"], ["admin-health", "Health"], ["admin-token-management", "Tokens / recent audit"]],
    credentials: [["credentials", "Credentials"], ["admin-persistent-audit", "Persistent audit"]],
    "admin-backups": [["admin-backups", "Snapshots / restore"], ["admin-backup-schedule", "Scheduled backups"]],
    webhooks: [["webhooks", "All recovery integrations"], ["admin-webhooks", "Subscriptions"], ["admin-pitr", "Point-in-time restore"]]
  };
  const routes = {
    database: ["overview", "database"],
    browse: ["collections", "browse"],
    editor: ["collections", "editor", "editor"],
    "write-json": ["collections", "write-json", "editor"],
    index: ["collections", "index"],
    collections: ["collections", "collections"],
    "collection-search": ["collections", "search", "collection-search"],
    "collection-retrieval": ["collections", "retrieval", "collection-search"],
    "collection-explore": ["collections", "explore", "collection-search"],
    search: ["search", "search"],
    retrieval: ["search", "retrieval"],
    explore: ["search", "explore"],
    observability: ["observability", "observability"],
    diagnostics: ["observability", "diagnostics"],
    "change-feed": ["observability", "change-feed"],
    operations: ["administration", "operations"],
    credentials: ["administration", "credentials"],
    webhooks: ["administration", "webhooks"],
    tenants: ["administration", "tenants"],
    users: ["administration", "users"],
    replication: ["administration", "replication"],
    "transport-security": ["administration", "transport-security"],
    "admin-health": ["administration", "operations", "operations"],
    "admin-token-management": ["administration", "operations", "operations"],
    "admin-backups": ["administration", "operations", "admin-backups"],
    "admin-backup-schedule": ["administration", "operations", "admin-backups"],
    "admin-auto-maintenance": ["administration", "operations", "admin-auto-maintenance"],
    "admin-persistent-audit": ["administration", "credentials", "credentials"],
    "admin-webhooks": ["administration", "webhooks", "webhooks"],
    "admin-pitr": ["administration", "webhooks", "webhooks"],
    requests: ["developer", "requests"],
    "data-tools": ["developer", "data-tools"],
    "resumable-import": ["developer", "resumable-import"]
  };

  function normalizeTarget(target) {
    try { return decodeURIComponent(String(target || "").replace(/^#/, "")); }
    catch (_) { return ""; }
  }

  function resolveRoute(target) {
    const id = normalizeTarget(target);
    const route = Object.prototype.hasOwnProperty.call(routes, id) ? routes[id] : routes.database;
    return { id: routes[id] === route ? id : "database", workspace: route[0], section: route[1], tab: route[2] || route[1] };
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
    const target = document.getElementById(normalizeTarget(targetId));
    const containing = target && target.closest("main > section");
    const route = resolveRoute(Object.prototype.hasOwnProperty.call(routes, normalizeTarget(targetId)) ? targetId : containing && containing.id);
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
    const secondary = route.workspace === "collections" ? route.tab === "editor" ? collectionWrite : route.tab === "collection-search" ? collectionSearch : [] : route.workspace === "administration" ? adminTabs[route.tab] || [] : [];
    renderTabs(subtabs, secondary, route.id, route.workspace === "administration" ? "Administration tools" : "Collection tools");
    document.querySelectorAll("[data-admin-view]").forEach(panel => {
      panel.hidden = route.workspace === "administration" && route.id.startsWith("admin-") && panel.closest("main > section")?.id === route.section && panel.id !== route.id;
    });
    const visible = document.getElementById(route.section);
    visible.setAttribute("role", "tabpanel");
    const selectedLink = (secondary.length ? subtabs : tabs).querySelector('[aria-selected="true"]');
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

  api.reveal = function (targetId) {
    const id = normalizeTarget(targetId);
    const target = document.getElementById(id);
    if (!target && !Object.prototype.hasOwnProperty.call(routes, id)) return null;
    const containing = target && target.closest("main > section");
    if (containing && current && containing.id === current.section
      && (!Object.prototype.hasOwnProperty.call(routes, id) || current.id === id)
      && !target.closest("[data-admin-view]")?.hidden) {
      openDetails(target);
      const panel = target.closest("#record-inspector > [role='tabpanel']");
      if (panel) inspectorTab(panel.id.replace("record-inspector-", ""));
      return containing;
    }
    if (target && !containing && !Object.prototype.hasOwnProperty.call(routes, id)) {
      openDetails(target);
      return target;
    }
    return navigate(id, { scrollTarget: false, keepScroll: true });
  };
  root.VLiteNavigation = api;
  if ("scrollRestoration" in root.history) root.history.scrollRestoration = "manual";

  document.addEventListener("click", event => {
    const inspector = event.target.closest("[data-record-tab]");
    if (inspector) { inspectorTab(inspector.dataset.recordTab); return; }
    const link = event.target.closest('a[href^="#"]');
    if (!link || event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    const id = normalizeTarget(link.getAttribute("href"));
    const target = document.getElementById(id);
    if (!Object.prototype.hasOwnProperty.call(routes, id) && !(target && target.closest("main > section"))) return;
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
