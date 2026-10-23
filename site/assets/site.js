(() => {
  const root = document.documentElement;
  const stored = (() => { try { return localStorage.getItem("vlite-theme"); } catch { return null; } })();
  const system = window.matchMedia("(prefers-color-scheme: dark)");
  const apply = (theme) => root.setAttribute("data-theme", theme);
  apply(stored || (system.matches ? "dark" : "light"));
  system.addEventListener("change", (event) => { if (!stored) apply(event.matches ? "dark" : "light"); });

  document.addEventListener("DOMContentLoaded", () => {
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
  });
})();
