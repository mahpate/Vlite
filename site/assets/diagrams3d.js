// SVGs own every label and connection. One optional renderer adds light and depth.
const root = document.documentElement;
const pictures = [...document.querySelectorAll(".deployment-options picture, figure[data-diagram] picture")];
const motion = matchMedia("(prefers-reduced-motion: reduce)");
const force = new URLSearchParams(location.search).has("force3d");
const roles = ["interface", "search", "storage", "operations", "error"];
const colors = ["#8b5cf6", "#0891b2", "#059669", "#d97706", "#dc2626"];
let renderer, canvas, raf = 0, dead = false, previous = 0, elapsed = 0;
const views = [];
const cleanups = [];
const state = (value, reason = "") => {
  root.dataset.diagrams3d = value;
  root.dataset.diagrams3dReason = reason;
};

function stop(reason) {
  if (dead) return;
  dead = true;
  cancelAnimationFrame(raf);
  cleanups.splice(0).forEach(fn => fn());
  for (const view of views) {
    view.picture.classList.remove("diagram-3d-stage");
    view.scene.traverse(object => {
      object.geometry?.dispose();
      object.material?.dispose();
    });
  }
  renderer?.dispose();
  renderer?.forceContextLoss();
  canvas?.remove();
  state("off", reason);
}

function schedule() {
  if (!raf && !dead) raf = requestAnimationFrame(frame);
}

function frame(now) {
  raf = 0;
  if (dead || document.hidden) { previous = 0; return; }
  if (previous && now - previous < 33 && !motion.matches) { schedule(); return; }
  if (previous && !motion.matches) elapsed += Math.min((now - previous) / 1000, 0.1);
  previous = now;
  try {
    const width = innerWidth, height = innerHeight;
    if (canvas.clientWidth !== width || canvas.clientHeight !== height ||
        renderer.domElement.width !== Math.floor(width * renderer.getPixelRatio())) {
      renderer.setSize(width, height);
    }
    renderer.setScissorTest(false);
    renderer.clear();
    renderer.setScissorTest(true);
    let active = false;
    for (const view of views) {
      const box = view.picture.getBoundingClientRect();
      if (box.bottom <= 0 || box.top >= height || box.right <= 0 || box.left >= width || !box.width || !box.height) continue;
      active = true;
      const image = view.image.getBoundingClientRect();
      const scale = Math.min(image.width / view.vb.width, image.height / view.vb.height);
      const ox = image.left - box.left + (image.width - view.vb.width * scale) / 2;
      const oy = image.top - box.top + (image.height - view.vb.height * scale) / 2;
      view.camera.aspect = box.width / box.height;
      view.camera.position.z = box.height / (2 * Math.tan(Math.PI / 8));
      view.camera.updateProjectionMatrix();
      view.stage.scale.set(box.width, box.height, 1);
      view.stage.material.uniforms.uDark.value = root.dataset.theme === "dark" ? 1 : 0;
      view.floor.scale.set(box.width * 0.94, 40, 1);
      view.floor.position.set(0, -box.height / 2 + 11, -9);
      for (const halo of view.halos) {
        const b = halo.bounds;
        halo.mesh.position.set(ox + (b.x - view.vb.x + b.width / 2) * scale - box.width / 2,
          box.height / 2 - oy - (b.y - view.vb.y + b.height / 2) * scale, 0.4);
        halo.mesh.scale.set(b.width * scale + 12, b.height * scale + 12, 1);
        halo.mesh.material.uniforms.uSize.value = [b.width * scale + 12, b.height * scale + 12];
      }
      const sequenceStep = Math.floor(elapsed / 1.25) % view.edges.length;
      const positions = view.pulses.geometry.getAttribute("position");
      for (let i = 0; i < view.edges.length; i++) {
        const edge = view.edges[i];
        const phase = motion.matches ? 0.5 : view.sequence ? (elapsed / 1.25) % 1 : (elapsed * 0.28 + i * 0.27) % 1;
        const offset = phase * (edge.length - 1);
        const a = edge[Math.floor(offset)], b = edge[Math.min(Math.floor(offset) + 1, edge.length - 1)];
        const t = offset % 1;
        const x = a.x + (b.x - a.x) * t, y = a.y + (b.y - a.y) * t;
        positions.setXYZ(i, ox + (x - view.vb.x) * scale - box.width / 2,
          box.height / 2 - oy - (y - view.vb.y) * scale,
          view.sequence && i !== (motion.matches ? 0 : sequenceStep) ? -10000 : 1);
      }
      positions.needsUpdate = true;
      view.picture.dataset.sequenceStep = String(motion.matches ? 0 : sequenceStep);
      renderer.setViewport(box.left, height - box.bottom, box.width, box.height);
      renderer.setScissor(Math.max(0, box.left), Math.max(0, height - box.bottom),
        Math.min(width, box.right) - Math.max(0, box.left),
        Math.min(height, box.bottom) - Math.max(0, box.top));
      renderer.render(view.scene, view.camera);
      view.picture.dataset.renderCount = String(Number(view.picture.dataset.renderCount || 0) + 1);
    }
    if (active && !motion.matches) schedule();
    else previous = 0;
  } catch { stop("render-error"); }
}

// Measure only local SVG geometry, never a hand-authored alternate diagram.
async function geometry(image) {
  const url = new URL(image.dataset.light || image.currentSrc, location.href);
  if (url.origin !== location.origin) throw new Error("non-local-svg");
  const response = await fetch(url, { credentials: "same-origin", mode: "same-origin" });
  if (!response.ok) throw new Error("svg-unavailable");
  const doc = new DOMParser().parseFromString(await response.text(), "image/svg+xml");
  const svg = doc.documentElement;
  if (svg.localName !== "svg" || svg.querySelector("parsererror, script, foreignObject") ||
      [...svg.querySelectorAll("*")].some(e => [...e.attributes].some(a =>
        /^on/i.test(a.name) || (/^(href|src|xlink:href)$/.test(a.name) && !a.value.startsWith("#"))))) {
    throw new Error("unsupported-svg");
  }
  const holder = document.createElement("div");
  holder.className = "diagram-3d-measure";
  holder.setAttribute("aria-hidden", "true");
  holder.append(document.importNode(svg, true));
  document.body.append(holder);
  try {
    const measured = holder.firstChild;
    const vb = measured.viewBox.baseVal;
    measured.style.width = `${vb.width}px`;
    measured.style.height = `${vb.height}px`;
    measured.style.maxWidth = "none";
    const inverse = measured.getCTM().inverse();
    const convert = element => inverse.multiply(element.getCTM());
    const sequence = measured.getAttribute("aria-roledescription") === "sequence";
    const edges = [];
    const selector = sequence ? ".messageLine0, .messageLine1" : ".flowchart-link, [marker-end]";
    for (const original of measured.querySelectorAll(selector)) {
      if (!(original instanceof SVGGeometryElement) || original.closest("defs")) continue;
      // Disjoint SVG subpaths are distinct routes, not an invented bridge between them.
      const parts = original.localName === "path" ? (original.getAttribute("d").match(/[Mm][^Mm]*/g) || []) : [null];
      for (const part of parts) {
        const edge = part ? original.cloneNode() : original;
        if (part) { edge.setAttribute("d", part); original.after(edge); }
        try {
          const length = edge.getTotalLength(), matrix = convert(edge);
          if (!length) continue;
          edges.push(Array.from({ length: 97 }, (_, i) =>
            edge.getPointAtLength(length * i / 96).matrixTransform(matrix)));
        } finally { if (part) edge.remove(); }
      }
    }
    if (sequence) edges.sort((a, b) => a[0].y - b[0].y);
    const halos = [];
    for (const element of measured.querySelectorAll(".node .label-container, rect.actor, rect[stroke], path[stroke], ellipse[stroke]")) {
      if (element.closest("defs") || element.hasAttribute("marker-end")) continue;
      const stroke = element.getAttribute("style")?.match(/stroke:\s*(#[a-f0-9]{6})/i)?.[1] ||
        element.getAttribute("stroke");
      let role = colors.indexOf(stroke?.toLowerCase());
      if (role < 0 && element.classList.contains("actor")) {
        const name = element.getAttribute("name") || "";
        role = /^(F|Idx|D)$/.test(name) ? (name === "Idx" ? 1 : 2) : /^(DB|Enc)$/.test(name) ? 3 : 0;
      }
      if (role < 0) continue;
      const b = element.getBBox(), matrix = convert(element);
      const a = new DOMPoint(b.x, b.y).matrixTransform(matrix);
      const z = new DOMPoint(b.x + b.width, b.y + b.height).matrixTransform(matrix);
      halos.push({ bounds: { x: a.x, y: a.y, width: z.x - a.x, height: z.y - a.y }, role });
    }
    return { vb: { x: vb.x, y: vb.y, width: vb.width, height: vb.height }, edges, halos, sequence };
  } finally { holder.remove(); }
}

async function start() {
  if (!pictures.length) return;
  if (navigator.connection?.saveData) { state("off", "save-data"); return; }
  state("loading");
  canvas = document.createElement("canvas");
  canvas.className = "diagrams-3d-canvas";
  canvas.setAttribute("aria-hidden", "true");
  let gl;
  try { gl = canvas.getContext("webgl2", { alpha: true, antialias: true, powerPreference: "low-power", failIfMajorPerformanceCaveat: !force }); } catch { /* unchanged SVG fallback */ }
  if (!gl) { stop("no-webgl2"); return; }
  const info = gl.getExtension("WEBGL_debug_renderer_info");
  const device = String(gl.getParameter(info ? info.UNMASKED_RENDERER_WEBGL : gl.RENDERER));
  if (!force && /swiftshader|llvmpipe|softpipe|software|basic render/i.test(device)) {
    gl.getExtension("WEBGL_lose_context")?.loseContext();
    stop("software-gl");
    return;
  }
  try {
    const THREE = await import("./vendor/three/three.min.js");
    renderer = new THREE.WebGLRenderer({ canvas, context: gl, alpha: true, antialias: true });
    renderer.autoClear = false;
    renderer.setClearColor(0, 0);
    renderer.setPixelRatio(Math.min(devicePixelRatio, 1.5));
    renderer.setSize(innerWidth, innerHeight);
    document.body.append(canvas);
    const vertex = `varying vec2 vUv; void main(){vUv=uv; gl_Position=projectionMatrix*modelViewMatrix*vec4(position,1.);}`;
    const material = (fragmentShader, uniforms = {}) => new THREE.ShaderMaterial({
      vertexShader: vertex, fragmentShader, uniforms, transparent: true, depthTest: false, depthWrite: false,
    });
    const haloFragment = `varying vec2 vUv; uniform vec3 uColor; uniform vec2 uSize;
      void main(){vec2 q=abs((vUv-.5)*uSize)-(uSize*.5-vec2(12.));
      float d=length(max(q,0.))+min(max(q.x,q.y),0.)-6.;
      gl_FragColor=vec4(uColor,exp(-abs(d)*.32)*.27);}`;
    const stageFragment = `varying vec2 vUv; uniform float uDark;
      void main(){vec2 p=abs(vUv-.5)*2.; float d=max(p.x,p.y);
      float rim=smoothstep(.93,.985,d)*(1.-smoothstep(.985,1.,d));
      float light=exp(-length((vUv-vec2(.15,.85))*vec2(2.,1.))*5.);
      gl_FragColor=vec4(mix(vec3(.20,.36,.78),vec3(.39,.65,1.),uDark),rim*.20+light*.055);}`;
    const floorFragment = `varying vec2 vUv; void main(){
      vec2 grid=abs(fract(vUv*vec2(22.,5.))-.5);
      float line=1.-smoothstep(.025,.055,min(grid.x,grid.y));
      gl_FragColor=vec4(.30,.52,.86,(line*.25+.04)*sin(vUv.x*3.14159));}`;
    const parsed = await Promise.all(pictures.map(async picture => {
      try { return { picture, image: picture.querySelector("img"), ...await geometry(picture.querySelector("img")) }; }
      catch { return null; } // A single unavailable diagram must not disable the others.
    }));
    for (const data of parsed.filter(Boolean)) {
      if (!data.edges.length) continue;
      const scene = new THREE.Scene();
      const camera = new THREE.PerspectiveCamera(45, 1, 0.1, 20000);
      const stage = new THREE.Mesh(new THREE.PlaneGeometry(1, 1), material(stageFragment, { uDark: { value: 0 } }));
      stage.position.z = -4;
      scene.add(stage);
      const floor = new THREE.Mesh(new THREE.PlaneGeometry(1, 1), material(floorFragment));
      floor.rotation.x = -0.65;
      scene.add(floor);
      const halos = data.halos.map(({ bounds, role }) => {
        const color = (getComputedStyle(root).getPropertyValue(`--diagram-${roles[role]}`).trim() || colors[role]).slice(1);
        const n = parseInt(color, 16);
        const mesh = new THREE.Mesh(new THREE.PlaneGeometry(1, 1), material(haloFragment, {
          uColor: { value: [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255] },
          uSize: { value: [1, 1] },
        }));
        mesh.userData.role = roles[role];
        scene.add(mesh);
        return { mesh, bounds };
      });
      const points = new THREE.BufferGeometry();
      points.setAttribute("position", new THREE.BufferAttribute(new Float32Array(data.edges.length * 3), 3));
      const pulseMaterial = new THREE.ShaderMaterial({
        vertexShader: `uniform float uDpr; void main(){gl_Position=projectionMatrix*modelViewMatrix*vec4(position,1.); gl_PointSize=15.*uDpr;}`,
        fragmentShader: `void main(){float r=length(gl_PointCoord-.5)*2.;
          float a=(1.-smoothstep(.1,1.,r)); gl_FragColor=vec4(.18,.65,.95,a*.9);}`,
        uniforms: { uDpr: { value: renderer.getPixelRatio() } }, transparent: true, depthTest: false, depthWrite: false,
      });
      const pulses = new THREE.Points(points, pulseMaterial);
      pulses.frustumCulled = false;
      scene.add(pulses);
      data.picture.classList.add("diagram-3d-stage");
      data.picture.dataset.edgeCount = String(data.edges.length);
      data.picture.dataset.haloCount = String(halos.length);
      data.picture.dataset.sequence = String(data.sequence);
      views.push({ ...data, scene, camera, stage, floor, halos, pulses });
    }
    if (!views.length) { stop("no-geometry"); return; }
    const listen = (target, event, fn) => {
      target.addEventListener(event, fn, { passive: true });
      cleanups.push(() => target.removeEventListener(event, fn));
    };
    listen(window, "scroll", schedule);
    listen(window, "resize", schedule);
    listen(document, "visibilitychange", () => { previous = 0; schedule(); });
    listen(motion, "change", () => { previous = 0; schedule(); });
    listen(canvas, "webglcontextlost", () => stop("context-lost"));
    listen(window, "pagehide", () => stop("pagehide"));
    const theme = new MutationObserver(schedule);
    theme.observe(root, { attributes: true, attributeFilter: ["data-theme"] });
    cleanups.push(() => theme.disconnect());
    const resize = new ResizeObserver(schedule);
    pictures.forEach(picture => { resize.observe(picture); listen(picture.querySelector("img"), "load", schedule); });
    cleanups.push(() => resize.disconnect());
    if (navigator.connection) listen(navigator.connection, "change", () => {
      if (navigator.connection.saveData) stop("save-data");
    });
    state("ready");
    schedule();
  } catch { stop("initialization-error"); }
}
start();
