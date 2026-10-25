(() => {
  const root = document.documentElement;
  const stored = (() => { try { return localStorage.getItem("vlite-theme"); } catch { return null; } })();
  const system = window.matchMedia("(prefers-color-scheme: dark)");
  const apply = (theme) => root.setAttribute("data-theme", theme);
  apply(stored || (system.matches ? "dark" : "light"));
  system.addEventListener("change", (event) => { if (!stored) apply(event.matches ? "dark" : "light"); });

  document.addEventListener("DOMContentLoaded", () => {
    const updateDiagrams = () => {
      const dark = root.getAttribute("data-theme") === "dark";
      document.querySelectorAll("img[data-light][data-dark]").forEach((image) => {
        const source = image.parentElement.querySelector("source");
        if (source) source.media = dark ? "all" : "not all";
        image.src = dark ? image.dataset.dark : image.dataset.light;
      });
      document.querySelectorAll("a.diagram-full[data-light][data-dark]").forEach((link) => {
        link.href = dark ? link.dataset.dark : link.dataset.light;
      });
    };
    updateDiagrams();
    new MutationObserver(updateDiagrams).observe(root, { attributes: true, attributeFilter: ["data-theme"] });
    const toggle = document.getElementById("theme-toggle");
    if (toggle) {
      toggle.addEventListener("click", () => {
        const next = root.getAttribute("data-theme") === "dark" ? "light" : "dark";
        apply(next);
        try { localStorage.setItem("vlite-theme", next); } catch { /* private mode */ }
      });
    }

    document.querySelectorAll("[role=tablist]").forEach((list) => {
      const tabs = [...list.querySelectorAll("[role=tab]")];
      const select = (tab) => {
        tabs.forEach((t) => {
          const selected = t === tab;
          t.setAttribute("aria-selected", String(selected));
          t.tabIndex = selected ? 0 : -1;
          document.getElementById(t.getAttribute("aria-controls")).hidden = !selected;
        });
      };
      tabs.forEach((tab, index) => {
        tab.addEventListener("click", () => select(tab));
        tab.addEventListener("keydown", (event) => {
          const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
          if (step) { const next = tabs[(index + step + tabs.length) % tabs.length]; select(next); next.focus(); }
        });
      });
    });

    document.querySelectorAll("[data-copy]").forEach((button) => {
      button.addEventListener("click", async () => {
        const target = document.getElementById(button.dataset.copy);
        try {
          await navigator.clipboard.writeText(target.innerText.replace(/^\$ /gm, ""));
          button.textContent = "Copied";
        } catch {
          button.textContent = "Select & copy";
        }
        setTimeout(() => { button.textContent = "Copy"; }, 1500);
      });
    });

    const motion = window.matchMedia("(prefers-reduced-motion: reduce)");
    document.querySelectorAll("[data-guided-tour]").forEach((figure) => {
      const frames = [
        ["current-overview-light.png", "Overview: see a connected database, record count and index readiness."],
        ["current-token-help.png", "Connection help: find the token for your server."],
        ["current-collections.png", "Collections: organize vector spaces and choose their search settings."],
        ["current-records.png", "Browse: explore stored records and their typed metadata."],
        ["current-payload.png", "Payload: open a record and read its stored content."],
        ["current-metadata.png", "Metadata: inspect the fields used for filtering and grouping."],
        ["current-vector.png", "Vectors: inspect the numeric embedding behind a record."],
        ["current-write.png", "Write: prepare records, review validation and explicitly confirm changes."],
        ["current-index.png", "Indexes: inspect HNSW readiness and manage rebuilds."],
        ["current-search.png", "Search: run a vector query and compare ranked neighbors."],
        ["current-hybrid.png", "Hybrid search: combine vector similarity with text matches."],
        ["current-retrieval.png", "Advanced retrieval: recommend neighbors and count metadata facets."],
        ["current-explore.png", "Explore: compare records and inspect a sampled vector projection."],
        ["current-import.png", "Import and export: validate a sample file before committing records."],
        ["current-observability.png", "Observability: inspect real request metrics, routes and server logs."],
        ["current-feed.png", "Change feed: inspect committed events and page through their history."],
        ["current-administration.png", "Administration: inspect server health and manage operational controls."],
        ["current-backups.png", "Backups: inspect a consistent snapshot and available restore controls."],
        ["current-credentials.png", "Credentials: issue limited access, rotate secrets and inspect audit history."],
        ["current-tenants.png", "Tenants: create isolated namespaces and set their vector limits."],
        ["current-automation.png", "Automation: inspect and configure scheduled index maintenance."],
        ["current-webhooks.png", "Webhooks: manage change notifications and inspect delivery status."],
        ["current-tls.png", "TLS certificates: reload configured files; this HTTP demo explains why reload is unavailable."],
        ["current-replication.png", "Replication: export a bounded log segment for a separately managed replica."],
        ["current-developer.png", "Developer Console: explore requests, responses and API examples."],
        ["current-preferences.png", "Preferences: change theme and spacing from the top bar."],
      ];
      const image = figure.querySelector("img");
      const link = figure.querySelector("[data-tour-full]");
      const caption = figure.querySelector("[data-tour-caption]");
      const toggle = figure.querySelector("[data-tour-toggle]");
      const next = figure.querySelector("[data-tour-next]");
      const position = figure.querySelector("[data-tour-position]");
      const step = figure.querySelector("[data-tour-step]");
      frames.forEach((frame, number) => {
        const option = document.createElement("option");
        option.value = String(number);
        option.textContent = `${number + 1}. ${frame[1].split(":")[0]}`;
        step.append(option);
      });
      let index = 0;
      let enabled = !motion.matches;
      let visible = false;
      let timer;
      let failed = false;
      const show = () => {
        image.src = `assets/dashboard/${frames[index][0]}?v=connected`;
        image.alt = frames[index][1];
        link.href = image.src;
        caption.textContent = frames[index][1];
        position.textContent = `${index + 1} / ${frames.length}`;
        step.value = String(index);
      };
      const update = () => {
        clearTimeout(timer);
        toggle.textContent = enabled ? "Pause tour" : "Play tour";
        toggle.setAttribute("aria-pressed", String(enabled));
        if (enabled && visible && !document.hidden && !failed && image.complete && image.naturalWidth) {
          timer = setTimeout(() => {
            index = (index + 1) % frames.length;
            show();
          }, 1400);
        }
      };
      image.addEventListener("load", update);
      image.addEventListener("error", () => {
        failed = true;
        enabled = false;
        caption.textContent = "This view could not load. Use Next view to continue.";
        update();
      });
      toggle.addEventListener("click", () => {
        enabled = !enabled;
        if (failed) { failed = false; show(); }
        update();
      });
      next.addEventListener("click", () => {
        enabled = false;
        failed = false;
        index = (index + 1) % frames.length;
        show();
        update();
      });
      step.addEventListener("change", () => {
        enabled = false;
        failed = false;
        index = Number(step.value);
        show();
        update();
      });
      motion.addEventListener("change", () => { enabled = !motion.matches; update(); });
      document.addEventListener("visibilitychange", update);
      if ("IntersectionObserver" in window) {
        new IntersectionObserver(([entry]) => {
          visible = entry.isIntersecting;
          update();
        }, { threshold: 0.2 }).observe(figure);
      } else {
        visible = true;
      }
      figure.querySelector(".tour-controls").hidden = false;
      update();
    });
  });
})();
