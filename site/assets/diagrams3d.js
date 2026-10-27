// Progressive enhancement: the published SVGs remain the no-script / 2D view.
(() => {
  const scenes = [
    [".deployment-options article:nth-child(1)", [
      ["Your application", "Rust, Python, Node.js or C", "interface"],
      ["Embedded engine", "Database core and rebuildable indexes", "search"],
      ["Your database file", "Local persistent storage", "storage"],
    ]],
    [".deployment-options article:nth-child(2)", [
      ["Clients and dashboard", "REST or gRPC (graphs use REST)", "interface"],
      ["Your VLite server", "Access checks and one embedded engine", "operations"],
      ["Your database file", "One file per server on persistent storage", "storage"],
    ]],
    ['figure[data-diagram="layers"]', [
      ["SDKs and APIs", "One shared Database interface", "interface"],
      ["Database core", "Vectors, collections and property graphs", "operations"],
      ["Search and indexes", "Rebuildable caches, not authoritative data", "search"],
      ["Storage and file", "Checksummed commits to one .vlite file", "storage"],
    ]],
  ];
  for (const [selector, layers] of scenes) {
    const host = document.querySelector(selector);
    const picture = host?.querySelector("picture");
    if (!picture || !CSS.supports("transform-style", "preserve-3d")) continue;
    const panel = document.createElement("div");
    panel.className = "spatial-diagram";
    const controls = document.createElement("div");
    controls.className = "spatial-controls";
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "btn";
    toggle.textContent = "Show 3D layers";
    toggle.setAttribute("aria-expanded", "false");
    const stage = document.createElement("div");
    stage.className = "spatial-stage";
    stage.hidden = true;
    stage.id = `spatial-${layers.length}-${scenes.findIndex(item => item[0] === selector)}`;
    toggle.setAttribute("aria-controls", stage.id);
    const stack = document.createElement("ol");
    stack.className = "spatial-stack";
    stack.setAttribute("aria-label", "Layers from interface to authoritative file");
    layers.forEach(([title, detail, role], index) => {
      const layer = document.createElement("li");
      layer.className = `spatial-layer spatial-${role}`;
      layer.style.setProperty("--depth", `${(layers.length - 1 - index) * 55}px`);
      layer.style.setProperty("--order", index);
      const heading = document.createElement("strong");
      heading.textContent = `${index + 1}. ${title}`;
      const description = document.createElement("span");
      description.textContent = detail;
      layer.append(heading, description);
      stack.append(layer);
    });
    const label = document.createElement("label");
    label.textContent = "Rotate layers ";
    label.hidden = true;
    const angle = document.createElement("input");
    angle.type = "range";
    angle.min = "-35";
    angle.max = "35";
    angle.value = "-18";
    angle.setAttribute("aria-label", "Rotate 3D layers");
    angle.addEventListener("input", () => stack.style.setProperty("--turn", `${angle.value}deg`));
    label.append(angle);
    toggle.addEventListener("click", () => {
      const active = stage.hidden;
      stage.hidden = !active;
      picture.hidden = active;
      label.hidden = !active;
      toggle.textContent = active ? "Show 2D diagram" : "Show 3D layers";
      toggle.setAttribute("aria-expanded", String(active));
    });
    stage.append(stack);
    controls.append(toggle, label);
    panel.append(controls, stage);
    picture.before(panel);
  }
})();
