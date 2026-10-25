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
        ["current-overview-light.png", "Overview: connect to your database and see its status."],
        ["current-token-help.png", "Connection help: find the token for your server."],
        ["current-overview-dark.png", "Dark theme: choose the appearance that suits you."],
        ["current-preferences.png", "Preferences: change theme and spacing from the top bar."],
      ];
      const image = figure.querySelector("img");
      const link = figure.querySelector("[data-tour-full]");
      const caption = figure.querySelector("[data-tour-caption]");
      const toggle = figure.querySelector("[data-tour-toggle]");
      const next = figure.querySelector("[data-tour-next]");
      const position = figure.querySelector("[data-tour-position]");
      let index = 0;
      let enabled = !motion.matches;
      let visible = false;
      let timer;
      let failed = false;
      const show = () => {
        image.src = `assets/dashboard/${frames[index][0]}`;
        image.alt = frames[index][1];
        link.href = image.src;
        caption.textContent = frames[index][1];
        position.textContent = `${index + 1} / ${frames.length}`;
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
