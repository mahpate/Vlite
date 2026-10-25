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

    const motion = window.matchMedia("(prefers-reduced-motion: reduce)");
    const tours = [...document.querySelectorAll("[data-tour]")].map((button) => {
      const image = document.getElementById(button.getAttribute("aria-controls"));
      const status = button.parentElement.querySelector(".tour-status");
      const label = button.textContent;
      const accessibleLabel = button.getAttribute("aria-label");
      let timer;
      let playing = false;
      const stop = (message = "") => {
        playing = false;
        clearTimeout(timer);
        image.src = button.dataset.poster;
        button.textContent = label;
        button.setAttribute("aria-label", accessibleLabel);
        button.setAttribute("aria-pressed", "false");
        status.textContent = message;
      };
      image.addEventListener("load", () => {
        if (!playing) return;
        status.textContent = "Playing once. Stop returns to the still image.";
        timer = setTimeout(() => stop("Tour finished. Play again to replay."), Number(button.dataset.duration));
      });
      image.addEventListener("error", () => {
        // Do not retry a failed poster indefinitely.
        if (playing) stop("The animation could not load. Open the still image instead.");
      });
      button.addEventListener("click", () => {
        if (playing) { stop(); return; }
        if (motion.matches) return;
        tours.forEach((tour) => tour.stop());
        playing = true;
        button.textContent = "Stop tour";
        button.setAttribute("aria-label", accessibleLabel.replace("Play ", "Stop "));
        button.setAttribute("aria-pressed", "true");
        status.textContent = "Loading tour...";
        image.src = button.dataset.tour;
      });
      const updateMotion = () => {
        stop(motion.matches ? "Static preview: reduced motion is enabled." : "");
        button.disabled = motion.matches;
      };
      updateMotion();
      button.hidden = false;
      motion.addEventListener("change", updateMotion);
      return { stop, image };
    });
    document.addEventListener("visibilitychange", () => {
      if (document.hidden) tours.forEach((tour) => tour.stop());
    });
    if ("IntersectionObserver" in window) {
      const observer = new IntersectionObserver((entries) => {
        entries.filter((entry) => !entry.isIntersecting).forEach((entry) => {
          tours.find((tour) => tour.image === entry.target)?.stop();
        });
      });
      tours.forEach((tour) => observer.observe(tour.image));
    }
  });
})();
