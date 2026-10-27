// VLite landing page: an optional WebGL layer behind the hero illustration and the
// Toolkit cards. The SVG and HTML stay the source of truth. This module only adds
// depth and motion, and it removes itself if anything fails.

const hero = document.querySelector("body.landing .hero-split");
const map = hero ? hero.querySelector("svg.file-map") : null;
const root = document.documentElement;
const FORCE = new URLSearchParams(location.search).has("force3d");
const motionQuery = matchMedia("(prefers-reduced-motion: reduce)");
const finePointer = matchMedia("(pointer: fine)");

const DEG = Math.PI / 180;
const clamp = (v, lo = 0, hi = 1) => (v < lo ? lo : v > hi ? hi : v);
const lerp = (a, b, t) => a + (b - a) * t;
const smooth = (v) => { const x = clamp(v); return x * x * (3 - 2 * x); };
const easeOut = (v) => 1 - (1 - clamp(v)) ** 3;
const rng = (seed) => () => {
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
};
const gauss = (r) => Math.sqrt(-2 * Math.log(1 - r())) * Math.cos(2 * Math.PI * r());
const dist3 = (a, b) => Math.hypot(a[0] - b[0], a[1] - b[1], a[2] - b[2]);
const mix3 = (a, b, t, out = [0, 0, 0]) => { out[0] = lerp(a[0], b[0], t); out[1] = lerp(a[1], b[1], t); out[2] = lerp(a[2], b[2], t); return out; };
const isDark = () => root.getAttribute("data-theme") === "dark";

let probe = null;
function rgb(value, fallback) {
  const v = String(value || "").trim();
  let m = /^#([0-9a-f]{6})$/i.exec(v);
  if (m) { const n = parseInt(m[1], 16); return [(n >> 16) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255]; }
  m = /^#([0-9a-f]{3})$/i.exec(v);
  if (m) return rgb("#" + [...m[1]].map((c) => c + c).join(""), fallback);
  if (v) {
    try {
      if (!probe) { const c = document.createElement("canvas"); c.width = c.height = 1; probe = c.getContext("2d", { willReadFrequently: true }); }
      probe.clearRect(0, 0, 1, 1);
      probe.fillStyle = "#010203";
      probe.fillStyle = v;
      if (probe.fillStyle !== "#010203") {
        probe.fillRect(0, 0, 1, 1);
        const [r, g, b, a] = probe.getImageData(0, 0, 1, 1).data;
        if (a) return [r / 255, g / 255, b / 255];
      }
    } catch { /* use the fallback */ }
  }
  return fallback ? rgb(fallback) : [1, 1, 1];
}

const PALETTES = {
  light: { collections: "#059669", dense: "#0891b2", sparse: "#4f46e5", meta: "#8b5cf6", payload: "#d97706", graph: "#db2777", hi: "#312e81", star: "#4f46e5", accent: "#4f46e5", accent2: "#0891b2", good: "#059669" },
  dark: { collections: "#34d399", dense: "#22d3ee", sparse: "#818cf8", meta: "#a78bfa", payload: "#fbbf24", graph: "#f472b6", hi: "#e0f2fe", star: "#c7d2fe", accent: "#818cf8", accent2: "#22d3ee", good: "#34d399" },
};
const VARS = { collections: "--k-collections", dense: "--k-dense", sparse: "--k-sparse", meta: "--k-meta", payload: "--k-payload", graph: "--k-graph", accent: "--accent", accent2: "--accent-2", good: "--good" };
function palette() {
  const dark = isDark();
  const base = dark ? PALETTES.dark : PALETTES.light;
  const style = getComputedStyle(document.body);
  const out = { dark };
  for (const key of Object.keys(base)) out[key] = rgb(VARS[key] ? style.getPropertyValue(VARS[key]) : "", base[key]);
  return out;
}

// ---------------------------------------------------------------------------
// Lifecycle and scheduling

let THREE = null;
let dead = false;
let raf = 0;
const cleanups = [];
const views = [];
const themeHooks = [];
const pointer = { x: 0, y: 0, tx: 0, ty: 0 };
const CONTEXT = { alpha: true, antialias: true, premultipliedAlpha: true, depth: false, stencil: false, powerPreference: "low-power", failIfMajorPerformanceCaveat: !FORCE };

function setState(state, reason) {
  hero.setAttribute("data-3d", state);
  if (reason) hero.setAttribute("data-3d-reason", reason);
  else hero.removeAttribute("data-3d-reason");
}

function teardown(reason) {
  if (dead) return;
  dead = true;
  if (raf) cancelAnimationFrame(raf);
  raf = 0;
  while (cleanups.length) {
    try { cleanups.pop()(); } catch { /* keep tearing down */ }
  }
  views.length = 0;
  themeHooks.length = 0;
  setState("off", reason);
}

function fail(error) {
  console.warn("VLite 3D view turned off:", error);
  teardown("error");
}

function schedule() {
  if (!raf && !dead) raf = requestAnimationFrame(frame);
}

function markDirty() {
  for (const view of views) view.dirty = true;
  schedule();
}

function run(view, time) {
  try {
    view.render(time);
  } catch (error) {
    if (view.optional) { console.warn("VLite 3D cards turned off:", error); dropView(view); }
    else fail(error);
  }
}

function frame(now) {
  raf = 0;
  if (dead) return;
  const still = motionQuery.matches;
  let again = false;
  for (const view of views.slice()) {
    if (dead) return;
    if (!view.visible || document.hidden) continue;
    if (still) {
      if (view.dirty) { view.dirty = false; run(view, null); }
      continue;
    }
    again = true;
    if (view.last && now - view.last < 1000 / view.fps - 3) continue;
    const dt = view.last ? Math.min((now - view.last) / 1000, 0.05) : 0;
    view.last = now;
    view.time += dt;
    view.dirty = false;
    run(view, view.time);
  }
  if (again) schedule();
}

function addView(view, element) {
  Object.assign(view, { visible: true, dirty: true, last: 0, time: 0 });
  if (view.optional) views.unshift(view);
  else views.push(view);
  if ("IntersectionObserver" in window) {
    const io = new IntersectionObserver((entries) => {
      view.visible = entries[entries.length - 1].isIntersecting;
      view.last = 0;
      view.dirty = true;
      schedule();
    });
    io.observe(element);
    view.observers.push(io);
  }
  if ("ResizeObserver" in window) {
    const ro = new ResizeObserver(() => { view.dirty = true; schedule(); });
    ro.observe(element);
    view.observers.push(ro);
  }
}

function dropView(view) {
  const index = views.indexOf(view);
  if (index >= 0) views.splice(index, 1);
  for (const observer of view.observers) observer.disconnect();
  view.observers.length = 0;
  try { view.dispose(); } catch { /* already gone */ }
}

function context(canvas) {
  try { return canvas.getContext("webgl2", CONTEXT); } catch { return null; }
}

// Software WebGL (SwiftShader, llvmpipe, WARP) would animate on the CPU, so keep the SVG instead.
const SOFTWARE_GL = /swiftshader|llvmpipe|softpipe|software|basic render/i;
function softwareGL(gl) {
  try {
    let name = String(gl.getParameter(gl.RENDERER) || "");
    if (/^webkit webgl$/i.test(name)) {
      const info = gl.getExtension("WEBGL_debug_renderer_info");
      if (info) name = String(gl.getParameter(info.UNMASKED_RENDERER_WEBGL) || "");
    }
    return SOFTWARE_GL.test(name);
  } catch { return false; }
}

function makeRenderer(canvas, gl) {
  const renderer = new THREE.WebGLRenderer({ canvas, context: gl, alpha: true, premultipliedAlpha: true, antialias: true, depth: false, stencil: false, powerPreference: "low-power" });
  renderer.setClearColor(0x000000, 0);
  return renderer;
}

function material(vertexShader, fragmentShader, uniforms, options = {}) {
  return new THREE.ShaderMaterial({ vertexShader, fragmentShader, uniforms, transparent: true, depthWrite: false, depthTest: false, ...options });
}

function add(parent, object, order) {
  object.renderOrder = order;
  object.frustumCulled = false;
  parent.add(object);
  return object;
}

function attr(geometry, name, array, size, dynamic = false) {
  const attribute = new THREE.BufferAttribute(array instanceof Float32Array ? array : new Float32Array(array), size);
  if (dynamic) attribute.setUsage(THREE.DynamicDrawUsage);
  geometry.setAttribute(name, attribute);
  return attribute;
}

function geometry(attributes) {
  const geo = new THREE.BufferGeometry();
  for (const [name, array, size, dynamic] of attributes) attr(geo, name, array, size, dynamic);
  return geo;
}

// ---------------------------------------------------------------------------
// Shaders

const DRIFT_GLSL = `
uniform float uTime;
uniform float uDrift;
uniform float uIntro;
vec3 drift(float s) {
  return uDrift * vec3(sin(uTime * 0.61 + s * 12.9), sin(uTime * 0.47 + s * 7.3 + 1.7), sin(uTime * 0.53 + s * 4.1 + 3.1));
}
vec3 settle(vec3 p, float s) {
  vec3 away = p * 2.4 + vec3(sin(s * 91.7), cos(s * 53.3), sin(s * 17.9 + 1.0)) * 90.0;
  float k = clamp(uIntro * 1.35 - fract(s * 7.31) * 0.35, 0.0, 1.0);
  k = k * k * (3.0 - 2.0 * k);
  return mix(away, p, k) + drift(s);
}`;

const POINT_FRAG = `
uniform float uSharp;
varying vec4 vColor;
void main() {
  vec2 c = gl_PointCoord * 2.0 - 1.0;
  float r2 = dot(c, c);
  if (r2 > 1.0) discard;
  float a = mix(exp(-4.0 * r2), 1.0 - smoothstep(0.55, 1.0, r2), uSharp);
  gl_FragColor = vec4(vColor.rgb, vColor.a * a);
}`;

const COLOR_FRAG = `
varying vec4 vColor;
void main() { gl_FragColor = vColor; }`;

const CLUSTER_VERT = `
attribute float aKind;
attribute float aSize;
attribute float aSeed;
attribute float aHi;
uniform float uPx;
uniform float uScale;
uniform float uScan;
uniform float uScanY;
uniform float uHiMix;
uniform float uBoost;
uniform vec4 uPal[8];
varying vec4 vColor;
${DRIFT_GLSL}
void main() {
  vec4 mv = modelViewMatrix * vec4(settle(position, aSeed), 1.0);
  float d = (position.y - uScanY) / 6.0;
  float scan = uScan * exp(-d * d);
  vec4 base = uPal[int(aKind + 0.5)];
  float tw = 0.75 + 0.25 * sin(uTime * (0.7 + aSeed * 1.3) + aSeed * 60.0);
  float size = aSize * uBoost * (1.0 + 0.9 * aHi + 0.6 * scan) * uPx * uScale / -mv.z;
  vColor = vec4(mix(base.rgb, uPal[7].rgb, clamp(aHi * uHiMix + scan * 0.6, 0.0, 1.0)),
    mix(base.a * tw, 1.0, max(aHi, scan * 0.8)) * clamp(size / 1.5, 0.0, 1.0));
  gl_PointSize = max(size, 1.5);
  gl_Position = projectionMatrix * mv;
}`;

const LINK_VERT = `
attribute float aKind;
attribute float aA;
attribute float aSeed;
uniform vec4 uPal[8];
varying vec4 vColor;
${DRIFT_GLSL}
void main() {
  vColor = vec4(uPal[int(aKind + 0.5)].rgb, aA * clamp(uIntro * 2.0 - 1.0, 0.0, 1.0));
  gl_Position = projectionMatrix * modelViewMatrix * vec4(settle(position, aSeed), 1.0);
}`;

const FX_POINT_VERT = `
attribute vec4 aCol;
attribute float aSize;
uniform float uPx;
uniform float uScale;
varying vec4 vColor;
void main() {
  vec4 mv = modelViewMatrix * vec4(position, 1.0);
  float size = aSize * uPx * uScale / -mv.z;
  vColor = vec4(aCol.rgb, aCol.a * clamp(size / 1.5, 0.0, 1.0));
  gl_PointSize = max(size, 1.5);
  gl_Position = aCol.a > 0.001 ? projectionMatrix * mv : vec4(2.0, 2.0, 2.0, 1.0);
}`;

const FX_LINE_VERT = `
attribute vec4 aCol;
varying vec4 vColor;
void main() {
  vColor = aCol;
  gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
}`;

const RING_VERT = `
attribute float aSize;
attribute float aA;
uniform float uTime;
uniform float uPx;
uniform float uScale;
uniform float uHalf;
uniform float uAlpha;
uniform vec3 uColor;
uniform vec3 uHi;
varying vec4 vColor;
void main() {
  float a = position.y + position.z * uTime;
  vec4 mv = modelViewMatrix * vec4(cos(a) * position.x, sin(a) * position.x, 0.0, 1.0);
  float depth = clamp((mv.z + 1150.0) / 500.0, 0.0, 1.0);
  float sat = step(0.7, aA);
  float size = aSize * uPx * uScale / -mv.z;
  vColor = vec4(mix(uColor, uHi, sat * 0.6), uAlpha * aA * mix(0.25, 1.0, depth) * clamp(size / 1.5, 0.0, 1.0));
  gl_PointSize = max(size, 1.5);
  gl_Position = sin(a) * uHalf < 0.0 ? vec4(2.0, 2.0, 2.0, 1.0) : projectionMatrix * mv;
}`;

const STAR_VERT = `
attribute float aSeed;
uniform float uTime;
uniform float uPx;
uniform float uAlpha;
uniform vec2 uPar;
varying float vA;
void main() {
  vec2 xy = fract(position.xy + uPar * (0.2 + position.z)) * 2.0 - 1.0;
  float tw = 0.55 + 0.45 * sin(uTime * (0.5 + aSeed * 1.6) + aSeed * 40.0);
  vA = uAlpha * (0.25 + 0.75 * position.z) * tw;
  gl_PointSize = (0.8 + 1.9 * position.z) * uPx;
  gl_Position = vec4(xy, 0.0, 1.0);
}`;

const STAR_FRAG = `
uniform vec3 uColor;
varying float vA;
void main() {
  vec2 c = gl_PointCoord * 2.0 - 1.0;
  float r2 = dot(c, c);
  if (r2 > 1.0) discard;
  gl_FragColor = vec4(uColor, vA * (1.0 - r2));
}`;

const UV_VERT = `
varying vec2 vUv;
void main() {
  vUv = uv;
  gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
}`;

const HALO_FRAG = `
uniform vec3 uA;
uniform vec3 uB;
uniform float uAlpha;
varying vec2 vUv;
void main() {
  vec2 c = vUv * 2.0 - 1.0;
  gl_FragColor = vec4(mix(uA, uB, vUv.y), exp(-5.0 * dot(c, c)) * uAlpha);
}`;

const GLASS_VERT = `
varying vec3 vN;
varying vec3 vW;
varying vec3 vL;
void main() {
  vN = normalize(mat3(modelMatrix) * normal);
  vec4 w = modelMatrix * vec4(position, 1.0);
  vW = w.xyz;
  vL = position;
  gl_Position = projectionMatrix * viewMatrix * w;
}`;

const GLASS_FRAG = `
uniform vec3 uA;
uniform vec3 uB;
uniform vec3 uHi;
uniform float uBase;
uniform float uRim;
uniform float uGlint;
uniform float uGlintAt;
varying vec3 vN;
varying vec3 vW;
varying vec3 vL;
void main() {
  vec3 V = normalize(cameraPosition - vW);
  float fres = pow(clamp(1.0 - abs(dot(normalize(vN), V)), 0.0, 1.0), 2.2);
  vec3 col = mix(uA, uB, clamp((vL.y + 94.0) / 188.0, 0.0, 1.0));
  float band = (vL.x * 0.6 + vL.y - uGlintAt) / 14.0;
  float glint = exp(-band * band) * uGlint;
  col = mix(col, uHi, clamp(fres * 0.5 + glint * 0.6, 0.0, 1.0));
  gl_FragColor = vec4(col, uBase + fres * uRim + glint * 0.14);
}`;

const LOCAL_VERT = `
varying vec3 vL;
void main() {
  vL = position;
  gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
}`;

const EDGE_FRAG = `
uniform vec3 uColor;
uniform vec3 uHi;
uniform float uAlpha;
uniform float uGlint;
uniform float uGlintAt;
varying vec3 vL;
void main() {
  float front = mix(0.3, 1.0, smoothstep(-18.0, 18.0, vL.z));
  float band = (vL.x * 0.6 + vL.y - uGlintAt) / 14.0;
  float glint = exp(-band * band) * uGlint;
  gl_FragColor = vec4(mix(uColor, uHi, clamp(0.35 + glint * 0.5, 0.0, 1.0)), uAlpha * front + glint * 0.4);
}`;

const SCAN_FRAG = `
uniform vec3 uColor;
uniform float uScan;
uniform float uScanY;
varying vec3 vL;
void main() {
  float d = vL.y - uScanY;
  float line = exp(-d * d / 2.0);
  float tail = d > 0.0 ? exp(-d / 26.0) * 0.16 : 0.0;
  float a = (line * 0.9 + tail) * uScan;
  if (a < 0.003) discard;
  gl_FragColor = vec4(uColor, a);
}`;

const FLAT_VERT = `
void main() { gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0); }`;

const FLAT_FRAG = `
uniform vec3 uColor;
uniform float uAlpha;
void main() { gl_FragColor = vec4(uColor, uAlpha); }`;

const LANE_POINT_VERT = `
attribute float aSize;
attribute float aA;
attribute float aB;
uniform vec3 uCol;
uniform vec3 uHi;
uniform float uPx;
uniform float uScale;
varying vec4 vColor;
void main() {
  vec4 mv = modelViewMatrix * vec4(position, 1.0);
  float size = aSize * (1.0 + 0.6 * aB) * uPx * uScale / -mv.z;
  vColor = vec4(mix(uCol, uHi, aB), aA * clamp(size / 1.5, 0.0, 1.0));
  gl_PointSize = max(size, 1.5);
  gl_Position = aA > 0.001 ? projectionMatrix * mv : vec4(2.0, 2.0, 2.0, 1.0);
}`;

const LANE_LINE_VERT = `
attribute float aA;
attribute float aB;
uniform vec3 uCol;
uniform vec3 uHi;
uniform float uAlpha;
varying vec4 vColor;
void main() {
  vColor = vec4(mix(uCol, uHi, aB), aA * uAlpha);
  gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
}`;

const SOLID_VERT = `
varying vec3 vN;
void main() {
  vN = normalize(normalMatrix * normal);
  gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
}`;

const SOLID_FRAG = `
uniform vec3 uColor;
uniform float uAlpha;
varying vec3 vN;
void main() {
  float light = 0.62 + 0.38 * max(dot(normalize(vN), normalize(vec3(0.35, 0.8, 0.5))), 0.0);
  gl_FragColor = vec4(uColor * light, uAlpha);
}`;

// ---------------------------------------------------------------------------
// Hero: the database file as a glass crystal holding each kind of data

const W2 = 68;
const H2 = 94;
const RAD = 18;
const FOLD = 34;
const DEPTH = 36;
const D = 900;
const CYCLE = 9;
const STILL_T = 5.75;
const DRIFT = 0.9;
const KINDS = ["collections", "dense", "sparse", "meta", "payload", "graph"];
const ANCHORS = { collections: [0, 62, 0], dense: [-42, 34, 0], sparse: [42, 34, 0], meta: [-42, -32, 0], payload: [42, -32, 0], graph: [0, -63, 0] };
const QUERIES = [
  { kind: 1, q: [-50, 30, 3], from: [-150, 170, 60] },
  { kind: 2, q: [52, 30, -2], from: [150, 170, 60] },
];

function inside(x, y, z, m = 6) {
  if (Math.abs(x) > W2 - m || Math.abs(y) > H2 - m || Math.abs(z) > 14) return false;
  if (x + y > W2 + H2 - FOLD - 1.41 * m) return false;
  if (!(x > 0 && y > 0) && Math.abs(x) > W2 - RAD && Math.abs(y) > H2 - RAD && Math.hypot(Math.abs(x) - (W2 - RAD), Math.abs(y) - (H2 - RAD)) > RAD - m) return false;
  if (Math.abs(x) < 56 && y > -22 && y < 17) return false;
  return true;
}

function fileShape() {
  const shape = new THREE.Shape();
  shape.moveTo(-W2 + RAD, -H2);
  shape.lineTo(W2 - RAD, -H2);
  shape.quadraticCurveTo(W2, -H2, W2, -H2 + RAD);
  shape.lineTo(W2, H2 - FOLD);
  shape.lineTo(W2 - FOLD, H2);
  shape.lineTo(-W2 + RAD, H2);
  shape.quadraticCurveTo(-W2, H2, -W2, H2 - RAD);
  shape.lineTo(-W2, -H2 + RAD);
  shape.quadraticCurveTo(-W2, -H2, -W2 + RAD, -H2);
  return shape;
}

function buildClusters() {
  const r = rng(20250501);
  const pos = [];
  const kind = [];
  const size = [];
  const seed = [];
  const groups = [[], [], [], [], [], [], []];
  const push = (x, y, z, k, s) => { groups[k].push(kind.length); pos.push(x, y, z); kind.push(k); size.push(s); seed.push(r()); };
  for (const cx of [-33, -11, 11, 33]) {
    for (let i = 0; i < 3; i++) for (let j = 0; j < 3; j++) for (let k = 0; k < 2; k++) push(cx + (i - 1) * 4.5, 62 + (j - 1) * 4.5, (k - 0.5) * 4.5, 0, 2.3);
  }
  for (let n = 0, tries = 0; n < 520 && tries < 8000; tries++) {
    const x = -42 + gauss(r) * 9;
    const y = 34 + gauss(r) * 8;
    const z = gauss(r) * 6;
    if (inside(x, y, z)) { push(x, y, z, 1, 1.6 + r() * 0.6); n++; }
  }
  for (const angle of [20, 85, 150, 215, 330]) {
    const th = angle * DEG;
    const dz = (r() - 0.5) * 0.5;
    for (let i = 0; i < 28; i++) {
      const d = 2 + i * 0.85;
      const x = 42 + Math.cos(th) * d + (r() - 0.5) * 1.4;
      const y = 34 + Math.sin(th) * d + (r() - 0.5) * 1.4;
      const z = dz * d + (r() - 0.5) * 1.4;
      if (inside(x, y, z)) push(x, y, z, 2, 1.9);
    }
  }
  for (let i = 0; i < 7; i++) for (let j = 0; j < 5; j++) for (let k = 0; k < 2; k++) {
    const x = -42 + (i - 3) * 5;
    const y = -32 + (j - 2) * 5;
    const z = (k - 0.5) * 5;
    if (inside(x, y, z)) push(x, y, z, 3, 2.2);
  }
  for (const [cx, cy] of [[32, -29], [48, -24], [40, -36], [54, -38], [30, -41]]) {
    for (let n = 0, tries = 0; n < 30 && tries < 400; tries++) {
      const x = cx + gauss(r) * 2.5;
      const y = cy + gauss(r) * 2.5;
      const z = gauss(r) * 2.5;
      if (inside(x, y, z)) { push(x, y, z, 4, 2.0); n++; }
    }
  }
  const nodes = [];
  for (let tries = 0; nodes.length < 14 && tries < 4000; tries++) {
    const p = [(r() * 2 - 1) * 52, -82 + r() * 38, (r() * 2 - 1) * 12];
    if (inside(p[0], p[1], p[2]) && nodes.every((n) => dist3(n, p) >= 11)) nodes.push(p);
  }
  for (const [x, y, z] of nodes) push(x, y, z, 5, 3.6);
  for (let n = 0, tries = 0; n < 300 && tries < 4000; tries++) {
    const x = (r() * 2 - 1) * W2;
    const y = (r() * 2 - 1) * H2;
    const z = (r() * 2 - 1) * 14;
    if (inside(x, y, z, 3)) { push(x, y, z, 6, 1.0); n++; }
  }
  return { pos, kind, size, seed, groups };
}

function buildStory(data) {
  const { pos, groups } = data;
  const at = (i) => [pos[3 * i], pos[3 * i + 1], pos[3 * i + 2]];
  const gi = groups[5];
  const adj = gi.map(() => new Set());
  const edges = [];
  const link = (a, b) => { if (a !== b && !adj[a].has(b)) { adj[a].add(b); adj[b].add(a); edges.push([a, b]); } };
  for (let a = 1; a < gi.length; a++) {
    let best = 0;
    for (let b = 1; b < a; b++) if (dist3(at(gi[a]), at(gi[b])) < dist3(at(gi[a]), at(gi[best]))) best = b;
    link(a, best);
  }
  const pairs = [];
  for (let a = 0; a < gi.length; a++) for (let b = a + 1; b < gi.length; b++) pairs.push([dist3(at(gi[a]), at(gi[b])), a, b]);
  pairs.sort((x, y) => x[0] - y[0]);
  for (const [d, a, b] of pairs) { if (edges.length >= 20 || d >= 24) break; link(a, b); }
  const nearestNode = (p, accept = () => true) => {
    let best = -1;
    let bestD = Infinity;
    gi.forEach((g, k) => { const d = dist3(p, at(g)); if (accept(at(g)) && d < bestD) { bestD = d; best = k; } });
    return best;
  };
  const attachments = [];
  const queries = QUERIES.map((query) => {
    const knn = groups[query.kind].map((i) => [dist3(at(i), query.q), i]).sort((a, b) => a[0] - b[0]).slice(0, 6).map((e) => e[1]);
    const best = knn[0];
    const side = Math.sign(at(best)[0]);
    let node = nearestNode(at(best), (p) => Math.sign(p[0]) === side && Math.abs(p[0]) > 20);
    if (node < 0) node = nearestNode(at(best));
    attachments.push([best, node]);
    const l1 = [...adj[node]].slice(0, 4);
    const seen = new Set([node, ...l1]);
    const l2 = [];
    for (const parent of l1) {
      for (const child of adj[parent]) {
        if (l2.length < 6 && !seen.has(child)) { seen.add(child); l2.push([parent, child]); }
      }
    }
    const control = [lerp(query.from[0], query.q[0], 0.65), lerp(query.from[1], query.q[1], 0.15), lerp(query.from[2], query.q[2], 0.5)];
    return { ...query, knn, best, node, l1, l2, control };
  });
  const byRow = (k, pick) => groups[k].slice().sort((a, b) => pos[3 * a + 1] - pos[3 * b + 1] || pos[3 * a] - pos[3 * b]).filter((_, i) => pick.includes(i));
  for (const i of [...byRow(3, [0, 6]), ...byRow(4, [0, 9])]) {
    const node = nearestNode(at(i));
    if (node >= 0) attachments.push([i, node]);
  }
  return { gi, edges, attachments, queries, at };
}

function buildRings() {
  const r = rng(7);
  const data = [];
  const sizes = [];
  const alphas = [];
  const ring = (radius, count, speed, size, alpha) => {
    for (let i = 0; i < count; i++) { data.push(radius, (i / count) * Math.PI * 2, speed); sizes.push(size); alphas.push(alpha); }
  };
  ring(250, 220, 0.03, 1.7, 0.55);
  ring(272, 160, -0.02, 1.4, 0.4);
  for (let i = 0; i < 4; i++) {
    data.push(i % 2 ? 272 : 250, r() * Math.PI * 2, (0.15 + r() * 0.1) * (i % 2 ? -1 : 1));
    sizes.push(4.5);
    alphas.push(1);
  }
  return geometry([["position", data, 3], ["aSize", sizes, 1], ["aA", alphas, 1]]);
}

function buildStars() {
  const r = rng(99);
  const count = 1400;
  const pos = new Float32Array(count * 3);
  const seed = new Float32Array(count);
  for (let i = 0; i < count; i++) {
    pos[3 * i] = r();
    pos[3 * i + 1] = r();
    pos[3 * i + 2] = r() ** 2;
    seed[i] = r();
  }
  return geometry([["position", pos, 3], ["aSeed", seed, 1]]);
}

function createHero(renderer) {
  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(30, 1, 100, 2400);
  camera.position.set(0, 0, D);
  const rig = add(scene, new THREE.Group(), 0);
  const crystal = add(rig, new THREE.Group(), 0);
  const materials = [];
  const track = (m) => { materials.push(m); return m; };

  const U = {
    uTime: { value: 0 }, uDrift: { value: DRIFT }, uIntro: { value: 0 }, uPx: { value: 1 }, uScale: { value: D },
    uScan: { value: 0 }, uScanY: { value: 0 }, uHiMix: { value: 0.45 }, uSharp: { value: 0 }, uBoost: { value: 1 },
    uPal: { value: new Float32Array(32) }, uGlint: { value: 0 }, uGlintAt: { value: -200 },
  };
  const drift = { uTime: U.uTime, uDrift: U.uDrift, uIntro: U.uIntro };

  // Background stars in clip space.
  const starsMat = track(material(STAR_VERT, STAR_FRAG, { uTime: U.uTime, uPx: U.uPx, uAlpha: { value: 0.8 }, uPar: { value: [0, 0] }, uColor: { value: [1, 1, 1] } }));
  const stars = add(scene, new THREE.Points(buildStars(), starsMat), 0);

  // Soft halo and the orbit rings.
  const haloMat = track(material(UV_VERT, HALO_FRAG, { uA: { value: [1, 1, 1] }, uB: { value: [1, 1, 1] }, uAlpha: { value: 0.2 } }));
  const halo = add(rig, new THREE.Mesh(new THREE.PlaneGeometry(420, 420), haloMat), 1);
  halo.position.z = -60;
  const ringU = { uColor: { value: [1, 1, 1] }, uHi: { value: [1, 1, 1] }, uAlpha: { value: 0.5 } };
  const ringMat = (half) => track(material(RING_VERT, POINT_FRAG, { ...ringU, uTime: U.uTime, uPx: U.uPx, uScale: U.uScale, uSharp: U.uSharp, uHalf: { value: half } }));
  const ringOuter = add(rig, new THREE.Group(), 0);
  ringOuter.rotation.z = -18 * DEG;
  const ringInner = add(ringOuter, new THREE.Group(), 0);
  ringInner.rotation.x = -72 * DEG;
  const ringGeo = buildRings();
  add(ringInner, new THREE.Points(ringGeo, ringMat(1)), 2);
  add(ringInner, new THREE.Points(ringGeo, ringMat(-1)), 6);

  // The file: glass body, outline, folded corner and commit scan.
  const shape = fileShape();
  const glassGeo = new THREE.ExtrudeGeometry(shape, { depth: DEPTH, bevelEnabled: false, curveSegments: 10 });
  glassGeo.translate(0, 0, -DEPTH / 2);
  const glint = { uGlint: U.uGlint, uGlintAt: U.uGlintAt };
  const glassU = { uA: { value: [1, 1, 1] }, uB: { value: [1, 1, 1] }, uHi: { value: [1, 1, 1] } };
  const glassBack = track(material(GLASS_VERT, GLASS_FRAG, { ...glassU, ...glint, uBase: { value: 0.03 }, uRim: { value: 0.25 } }, { side: THREE.BackSide }));
  const glassFront = track(material(GLASS_VERT, GLASS_FRAG, { ...glassU, ...glint, uBase: { value: 0.05 }, uRim: { value: 0.55 } }, { side: THREE.FrontSide }));
  add(crystal, new THREE.Mesh(glassGeo, glassBack), 3);
  add(crystal, new THREE.Mesh(glassGeo, glassFront), 5);
  const edgeMat = track(material(LOCAL_VERT, EDGE_FRAG, { uColor: { value: [1, 1, 1] }, uHi: glassU.uHi, uAlpha: { value: 0.55 }, ...glint }));
  add(crystal, new THREE.LineSegments(new THREE.EdgesGeometry(glassGeo, 30), edgeMat), 6);
  const fz = DEPTH / 2 + 0.2;
  add(crystal, new THREE.LineSegments(geometry([["position", [W2 - FOLD, H2, fz, W2 - FOLD, H2 - FOLD, fz, W2 - FOLD, H2 - FOLD, fz, W2, H2 - FOLD, fz], 3]]), edgeMat), 6.2);
  const flatMat = track(material(FLAT_VERT, FLAT_FRAG, { uColor: edgeMat.uniforms.uColor, uAlpha: { value: 0.12 } }, { side: THREE.DoubleSide }));
  add(crystal, new THREE.Mesh(geometry([["position", [W2 - FOLD, H2, fz - 0.1, W2 - FOLD, H2 - FOLD, fz - 0.1, W2, H2 - FOLD, fz - 0.1], 3]]), flatMat), 6.1);
  const scanGeo = new THREE.ShapeGeometry(shape, 10);
  scanGeo.translate(0, 0, DEPTH / 2 + 0.6);
  const scanMat = track(material(LOCAL_VERT, SCAN_FRAG, { uColor: { value: [1, 1, 1] }, uScan: U.uScan, uScanY: U.uScanY }));
  add(crystal, new THREE.Mesh(scanGeo, scanMat), 4);

  // Data inside the file.
  const data = buildClusters();
  const story = buildStory(data);
  const count = data.kind.length;
  const hiAttr = new Float32Array(count);
  const pointGeo = geometry([["position", data.pos, 3], ["aKind", data.kind, 1], ["aSize", data.size, 1], ["aSeed", data.seed, 1], ["aHi", hiAttr, 1, true]]);
  const pointMat = track(material(CLUSTER_VERT, POINT_FRAG, { ...drift, uPx: U.uPx, uScale: U.uScale, uScan: U.uScan, uScanY: U.uScanY, uHiMix: U.uHiMix, uPal: U.uPal, uSharp: U.uSharp, uBoost: U.uBoost }));
  add(crystal, new THREE.Points(pointGeo, pointMat), 4.4);
  const linkPos = [];
  const linkKind = [];
  const linkA = [];
  const linkSeed = [];
  const pushLink = (i, j, kindI, kindJ, alpha) => {
    linkPos.push(...story.at(i), ...story.at(j));
    linkKind.push(kindI, kindJ);
    linkA.push(alpha, alpha);
    linkSeed.push(data.seed[i], data.seed[j]);
  };
  for (const [a, b] of story.edges) pushLink(story.gi[a], story.gi[b], 5, 5, 0.38);
  for (const [i, node] of story.attachments) pushLink(i, story.gi[node], data.kind[i], 5, 0.18);
  const linkGeo = geometry([["position", linkPos, 3], ["aKind", linkKind, 1], ["aA", linkA, 1], ["aSeed", linkSeed, 1]]);
  const linkMat = track(material(LINK_VERT, COLOR_FRAG, { ...drift, uPal: U.uPal }));
  add(crystal, new THREE.LineSegments(linkGeo, linkMat), 4.2);

  // Query effects, rewritten every frame.
  const MAX_LINES = 32;
  const MAX_POINTS = 48;
  const fxLineGeo = geometry([["position", new Float32Array(MAX_LINES * 6), 3, true], ["aCol", new Float32Array(MAX_LINES * 8), 4, true]]);
  const fxPointGeo = geometry([["position", new Float32Array(MAX_POINTS * 3), 3, true], ["aCol", new Float32Array(MAX_POINTS * 4), 4, true], ["aSize", new Float32Array(MAX_POINTS), 1, true]]);
  const fxLineMat = track(material(FX_LINE_VERT, COLOR_FRAG, {}));
  const fxPointMat = track(material(FX_POINT_VERT, POINT_FRAG, { uPx: U.uPx, uScale: U.uScale, uSharp: U.uSharp }));
  add(crystal, new THREE.LineSegments(fxLineGeo, fxLineMat), 4.6);
  add(crystal, new THREE.Points(fxPointGeo, fxPointMat), 4.8);
  const fx = { lines: 0, points: 0 };
  const fxLine = (a, b, col, alpha) => {
    if (fx.lines >= MAX_LINES || alpha <= 0) return;
    const p = fxLineGeo.attributes.position.array;
    const c = fxLineGeo.attributes.aCol.array;
    p.set(a, fx.lines * 6);
    p.set(b, fx.lines * 6 + 3);
    for (let k = 0; k < 2; k++) c.set([col[0], col[1], col[2], alpha], fx.lines * 8 + k * 4);
    fx.lines++;
  };
  const fxPoint = (at, col, alpha, size) => {
    if (fx.points >= MAX_POINTS || alpha <= 0) return;
    fxPointGeo.attributes.position.array.set(at, fx.points * 3);
    fxPointGeo.attributes.aCol.array.set([col[0], col[1], col[2], alpha], fx.points * 4);
    fxPointGeo.attributes.aSize.array[fx.points] = size;
    fx.points++;
  };
  const drifted = (i, t, out = [0, 0, 0]) => {
    const s = data.seed[i];
    out[0] = data.pos[3 * i] + DRIFT * Math.sin(t * 0.61 + s * 12.9);
    out[1] = data.pos[3 * i + 1] + DRIFT * Math.sin(t * 0.47 + s * 7.3 + 1.7);
    out[2] = data.pos[3 * i + 2] + DRIFT * Math.sin(t * 0.53 + s * 4.1 + 3.1);
    return out;
  };
  const bezier = (a, c, b, u, out) => {
    const v = 1 - u;
    for (let k = 0; k < 3; k++) out[k] = v * v * a[k] + 2 * v * u * c[k] + u * u * b[k];
    return out;
  };

  // SVG tethers: each chip's dashed edge follows its cluster inside the moving file.
  const status = map.querySelector(".fm-status");
  const nodes = [...map.querySelectorAll(".fm-node")];
  const tethers = [];
  for (const edge of map.querySelectorAll(".fm-edge")) {
    const d = edge.getAttribute("d") || "";
    const m = /^M\s*(-?[\d.]+)[\s,]+(-?[\d.]+)/.exec(d);
    if (!m) continue;
    const sx = Number(m[1]);
    const sy = Number(m[2]);
    const owner = nodes.find((node) => {
      const rect = node.querySelector("rect");
      if (!rect) return false;
      const x = Number(rect.getAttribute("x"));
      const y = Number(rect.getAttribute("y"));
      return sx >= x && sx <= x + Number(rect.getAttribute("width")) && sy >= y && sy <= y + Number(rect.getAttribute("height"));
    });
    const kind = owner && KINDS.find((k) => owner.classList.contains("fm-" + k));
    if (!kind) continue;
    const anchor = document.createElementNS("http://www.w3.org/2000/svg", "circle");
    anchor.setAttribute("class", "fm-anchor fm-" + kind);
    anchor.setAttribute("r", "3.2");
    anchor.setAttribute("cx", "240");
    anchor.setAttribute("cy", "212");
    map.insertBefore(anchor, nodes[0] || null);
    edge.classList.add("fm-" + kind);
    tethers.push({ edge, anchor, kind, d, sx, sy, local: new THREE.Vector3(...ANCHORS[kind]), last: "" });
  }
  let pulseTimer = 0;
  cleanups.push(() => {
    clearTimeout(pulseTimer);
    if (status) status.classList.remove("fm-commit");
    for (const t of tethers) { t.edge.setAttribute("d", t.d); t.edge.classList.remove("fm-" + t.kind); t.anchor.remove(); }
  });
  const pulse = () => {
    if (!status) return;
    status.classList.add("fm-commit");
    clearTimeout(pulseTimer);
    pulseTimer = setTimeout(() => status.classList.remove("fm-commit"), 900);
  };

  let P = null;
  function applyTheme() {
    P = palette();
    const dark = P.dark;
    const dust = P.accent.map((c, i) => (c + P.accent2[i]) / 2);
    const colors = [P.collections, P.dense, P.sparse, P.meta, P.payload, P.graph, dust, P.hi];
    const alphas = [0.9, 0.7, 0.85, 0.85, 0.85, 1, dark ? 0.35 : 0.3, 1];
    colors.forEach((c, i) => U.uPal.value.set([c[0], c[1], c[2], alphas[i]], i * 4));
    for (const m of materials) m.blending = dark ? THREE.AdditiveBlending : THREE.NormalBlending;
    U.uHiMix.value = dark ? 0.45 : 0;
    glassU.uA.value = P.accent;
    glassU.uB.value = P.accent2;
    glassU.uHi.value = P.hi;
    glassBack.uniforms.uBase.value = dark ? 0.03 : 0.04;
    glassBack.uniforms.uRim.value = dark ? 0.25 : 0.3;
    glassFront.uniforms.uBase.value = dark ? 0.05 : 0.06;
    glassFront.uniforms.uRim.value = dark ? 0.55 : 0.45;
    edgeMat.uniforms.uColor.value = P.accent;
    edgeMat.uniforms.uAlpha.value = dark ? 0.55 : 0.6;
    flatMat.uniforms.uAlpha.value = dark ? 0.14 : 0.1;
    starsMat.uniforms.uColor.value = dark ? P.star : P.accent;
    starsMat.uniforms.uAlpha.value = dark ? 0.8 : 0.35;
    haloMat.uniforms.uA.value = P.accent;
    haloMat.uniforms.uB.value = P.accent2;
    haloMat.uniforms.uAlpha.value = dark ? 0.22 : 0.1;
    ringU.uColor.value = dark ? P.accent2 : P.accent;
    ringU.uHi.value = P.hi;
    ringU.uAlpha.value = dark ? 0.5 : 0.45;
    scanMat.uniforms.uColor.value = P.good;
  }
  applyTheme();
  themeHooks.push(applyTheme);

  const cur = { W: 0, H: 0, pr: 0 };
  function layout() {
    const hr = hero.getBoundingClientRect();
    const mr = map.getBoundingClientRect();
    const W = Math.max(1, Math.round(hr.width));
    const H = Math.max(1, Math.round(hr.height));
    const shown = mr.width > 10 && mr.height > 10;
    const s = shown ? mr.width / 480 : 1;
    const cx = shown ? mr.left - hr.left + 240 * s : W / 2;
    const cy = shown ? mr.top - hr.top + 212 * s : H / 2;
    const pr = Math.min(devicePixelRatio || 1, 2, Math.sqrt(3.5e6 / (W * H)));
    if (pr !== cur.pr) { cur.pr = pr; renderer.setPixelRatio(pr); }
    if (W !== cur.W || H !== cur.H) { cur.W = W; cur.H = H; renderer.setSize(W, H, false); }
    camera.fov = (2 * Math.atan(H / (2 * s * D))) / DEG;
    camera.aspect = W / H;
    camera.setViewOffset(W, H, W / 2 - cx, H / 2 - cy, W, H);
    return { W, H, s, cx, cy, shown, pr, scroll: clamp(-hr.top / Math.max(1, hr.height)) };
  }

  const lit = [];
  let litBefore = 0;
  let lastCommit = -1;
  let prevT = 0;
  const tmp = [[0, 0, 0], [0, 0, 0], [0, 0, 0], [0, 0, 0]];
  const light = (i, v) => { if (v > hiAttr[i]) hiAttr[i] = v; lit.push(i); };

  function playStory(t, still) {
    for (const i of lit) hiAttr[i] = 0;
    lit.length = 0;
    fx.lines = 0;
    fx.points = 0;
    const T = still ? STILL_T : t % CYCLE;
    const n = still ? 0 : Math.floor(t / CYCLE);
    U.uScan.value = T < 1.8 ? Math.sin((Math.PI * T) / 1.8) : 0;
    U.uScanY.value = 98 - 196 * clamp(T / 1.8);
    if (!still && T >= 1.8 && n !== lastCommit) { lastCommit = n; pulse(); }
    const query = story.queries[n & 1];
    const fade = 1 - smooth((T - 5.8) / 1);
    if (T >= 2 && fade > 0) {
      const kc = query.kind === 1 ? P.dense : P.sparse;
      const gc = P.graph;
      if (T < 3.1) {
        const u = easeOut((T - 2) / 1.1);
        for (let i = 15; i >= 0; i--) {
          const v = u - i * 0.025;
          if (v < 0) continue;
          bezier(query.from, query.control, query.q, v, tmp[0]);
          if (i) fxPoint(tmp[0], kc, 0.7 * (1 - i / 16), 3.2 * (1 - i / 16));
          else fxPoint(tmp[0], P.hi, 1, 5);
        }
      } else {
        fxPoint(query.q, P.hi, fade, 4.2);
        query.knn.forEach((idx, i) => {
          const g = clamp((T - (3.1 + i * 0.12)) / 0.35);
          if (g <= 0) return;
          fxLine(query.q, mix3(query.q, drifted(idx, t, tmp[0]), g, tmp[1]), kc, 0.75 * fade);
          light(idx, g * fade);
        });
        if (T >= 4.3) {
          const h = smooth((T - 4.3) / 0.5);
          const B = drifted(query.best, t, tmp[0]);
          const G = drifted(story.gi[query.node], t, tmp[1]);
          const at = mix3(B, G, h, tmp[2]);
          fxLine(B, at, gc, 0.8 * fade);
          if (h < 1) fxPoint(at, gc, fade, 3.5);
          else light(story.gi[query.node], fade);
          if (T >= 4.8) {
            const g = smooth((T - 4.8) / 0.5);
            for (const k of query.l1) {
              fxLine(G, mix3(G, drifted(story.gi[k], t, tmp[2]), g, tmp[3]), gc, 0.9 * fade);
              light(story.gi[k], g * fade);
            }
          }
          if (T >= 5.3) {
            const g = smooth((T - 5.3) / 0.5);
            for (const [parent, child] of query.l2) {
              const from = drifted(story.gi[parent], t, tmp[2]);
              fxLine(from, mix3(from, drifted(story.gi[child], t, tmp[0]), g, tmp[3]), gc, 0.7 * fade);
              light(story.gi[child], 0.8 * g * fade);
            }
          }
        }
      }
    }
    if (lit.length || litBefore) pointGeo.attributes.aHi.needsUpdate = true;
    litBefore = lit.length;
    fxLineGeo.setDrawRange(0, fx.lines * 2);
    fxPointGeo.setDrawRange(0, fx.points);
    if (fx.lines) { fxLineGeo.attributes.position.needsUpdate = true; fxLineGeo.attributes.aCol.needsUpdate = true; }
    if (fx.points) { for (const name of ["position", "aCol", "aSize"]) fxPointGeo.attributes[name].needsUpdate = true; }
  }

  const v = new THREE.Vector3();
  const f = (x) => x.toFixed(1);
  function updateTethers(L) {
    for (const t of tethers) {
      v.copy(t.local).applyMatrix4(crystal.matrixWorld).project(camera);
      const ax = 240 + (((v.x + 1) / 2) * L.W - L.cx) / L.s;
      const ay = 212 + (((1 - v.y) / 2) * L.H - L.cy) / L.s;
      const dx = ax - t.sx;
      const d = `M${t.sx} ${t.sy}C${f(t.sx + dx * 0.55)} ${t.sy} ${f(ax - dx * 0.25)} ${f(ay + (t.sy - ay) * 0.35)} ${f(ax)} ${f(ay)}`;
      if (d === t.last) continue;
      t.last = d;
      t.edge.setAttribute("d", d);
      t.anchor.setAttribute("cx", f(ax));
      t.anchor.setAttribute("cy", f(ay));
    }
  }

  function render(time) {
    const still = time === null;
    const t = still ? STILL_T : time;
    const dt = still ? 0 : clamp(t - prevT, 0, 0.05);
    prevT = t;
    const L = layout();
    if (!still) {
      const k = 1 - Math.exp(-dt * 4);
      pointer.x += (pointer.tx - pointer.x) * k;
      pointer.y += (pointer.ty - pointer.y) * k;
    }
    const px = still ? 0 : pointer.x;
    const py = still ? 0 : pointer.y;
    rig.visible = L.shown;
    U.uScale.value = L.s * D;
    U.uPx.value = L.pr;
    // Soft additive points get lost below ~2 device px; firm them up on low-density screens.
    const low = Math.min(1, Math.max(0, (2 - L.pr) / 0.75));
    U.uSharp.value = P.dark ? 0.4 * low : 1;
    U.uBoost.value = P.dark ? 1 + 0.3 * low : 1;
    U.uTime.value = t;
    U.uIntro.value = still ? 1 : clamp(t / 1.6);
    crystal.rotation.x = 8 * DEG + (still ? 0 : 0.025 * Math.sin(0.27 * t)) + py * 6 * DEG;
    crystal.rotation.y = -18 * DEG + (still ? 0 : 0.05 * Math.sin(0.35 * t)) + px * 6 * DEG + 0.25 * L.scroll;
    rig.position.y = still ? 0 : 3 * Math.sin(0.5 * t);
    const par = starsMat.uniforms.uPar.value;
    par[0] = still ? 0 : px * 0.01 + t * 0.004;
    par[1] = still ? 0 : -py * 0.01 + L.scroll * 0.15;
    stars.geometry.setDrawRange(0, Math.round(clamp((L.W * L.H) / 900, 300, 1400) * (P.dark ? 1 : 0.4)));
    const gp = still ? 2 : (t % 7) / 2.2;
    U.uGlint.value = gp < 1 ? Math.sin(Math.PI * gp) : 0;
    U.uGlintAt.value = -170 + 340 * gp;
    playStory(t, still);
    scene.updateMatrixWorld();
    camera.updateMatrixWorld();
    renderer.render(scene, camera);
    if (L.shown) updateTethers(L);
    // With the file hidden on small screens only the slow star field remains.
    view.fps = L.shown ? 60 : 30;
  }

  const view = { fps: 60, optional: false, observers: [], render, dispose() {} };
  return view;
}

// ---------------------------------------------------------------------------
// Toolkit cards: one small scene per lane, drawn through scissored viewports

function laneUniforms() {
  return { uCol: { value: [1, 1, 1] }, uHi: { value: [1, 1, 1] }, uPx: { value: 1 }, uScale: { value: 100 }, uSharp: { value: 0 } };
}

function lanePoints(U, attributes, order, parent) {
  const geo = geometry(attributes);
  const mat = material(LANE_POINT_VERT, POINT_FRAG, { uCol: U.uCol, uHi: U.uHi, uPx: U.uPx, uScale: U.uScale, uSharp: U.uSharp });
  add(parent, new THREE.Points(geo, mat), order);
  return { geo, mat };
}

function laneLines(U, geo, alpha, order, parent, fill) {
  const n = geo.attributes.position.count;
  if (!geo.attributes.aA) attr(geo, "aA", new Float32Array(n).fill(fill ? fill[0] : 1), 1, true);
  if (!geo.attributes.aB) attr(geo, "aB", new Float32Array(n).fill(fill ? fill[1] : 0), 1, true);
  const mat = material(LANE_LINE_VERT, COLOR_FRAG, { uCol: U.uCol, uHi: U.uHi, uAlpha: { value: alpha } });
  add(parent, new THREE.LineSegments(geo, mat), order);
  return { geo, mat };
}

function sdkLane() {
  const U = laneUniforms();
  const scene = new THREE.Scene();
  const tilt = add(scene, new THREE.Group(), 0);
  tilt.rotation.x = 0.3;
  const spin = add(tilt, new THREE.Group(), 0);
  const materials = [];
  const coreGeo = new THREE.EdgesGeometry(new THREE.IcosahedronGeometry(0.9, 1));
  materials.push(laneLines(U, coreGeo, 0.8, 1, spin, [0.75, 0.3]).mat);
  const sats = [];
  for (let i = 0; i < 8; i++) { const a = (i / 8) * Math.PI * 2; sats.push([Math.cos(a) * 2.4, 0, Math.sin(a) * 2.4]); }
  const spokes = [];
  for (const s of sats) spokes.push(...s, ...s.map((c) => c * 0.4));
  materials.push(laneLines(U, geometry([["position", spokes, 3]]), 0.28, 2, spin, [1, 0]).mat);
  const count = 1 + 8 + 8;
  const pos = new Float32Array(count * 3);
  const size = new Float32Array(count);
  const alpha = new Float32Array(count).fill(1);
  const bright = new Float32Array(count);
  size[0] = 0.34; bright[0] = 0.85; alpha[0] = 0.9;
  sats.forEach((s, i) => { pos.set(s, (1 + i) * 3); size[1 + i] = 0.2; bright[1 + i] = 0.35; });
  for (let i = 0; i < 8; i++) size[9 + i] = 0.12;
  const pts = lanePoints(U, [["position", pos, 3, true], ["aSize", size, 1], ["aA", alpha, 1, true], ["aB", bright, 1]], 3, spin);
  materials.push(pts.mat);
  const update = (t) => {
    spin.rotation.y = t * 0.35;
    const p = pts.geo.attributes.position.array;
    const a = pts.geo.attributes.aA.array;
    for (let i = 0; i < 8; i++) {
      const u = (t * 0.45 + i / 8) % 1;
      const s = sats[i];
      p[(9 + i) * 3] = s[0] * lerp(1, 0.4, u);
      p[(9 + i) * 3 + 1] = s[1] * lerp(1, 0.4, u);
      p[(9 + i) * 3 + 2] = s[2] * lerp(1, 0.4, u);
      a[9 + i] = Math.sin(Math.PI * u) * 0.95;
    }
    pts.geo.attributes.position.needsUpdate = true;
    pts.geo.attributes.aA.needsUpdate = true;
  };
  return { U, scene, materials, update, still: 1.3, ex: 2.8, ey: 1.4, ez: 0.5, dir: [0, 0.3, 1] };
}

function searchLane() {
  const U = laneUniforms();
  const r = rng(11);
  const scene = new THREE.Scene();
  const group = add(scene, new THREE.Group(), 0);
  const materials = [];
  const Y = [-1.5, 0, 1.5];
  const base = [];
  for (let tries = 0; base.length < 48 && tries < 5000; tries++) {
    const a = r() * Math.PI * 2;
    const d = Math.sqrt(r()) * 3;
    const p = [Math.cos(a) * d, Math.sin(a) * d];
    if (base.every((q) => Math.hypot(q[0] - p[0], q[1] - p[1]) > 0.42)) base.push(p);
  }
  const layers = [base.map((_, i) => i)];
  layers[1] = layers[0].filter((_, i) => i % 3 === 0).slice(0, 16);
  layers[2] = layers[1].filter((_, i) => i % 3 === 0).slice(0, 5);
  const near = (l, i, k) => layers[l].filter((j) => j !== i).sort((a, b) => Math.hypot(base[a][0] - base[i][0], base[a][1] - base[i][1]) - Math.hypot(base[b][0] - base[i][0], base[b][1] - base[i][1])).slice(0, k);
  const adj = layers.map((ids, l) => {
    const m = new Map(ids.map((i) => [i, new Set()]));
    for (const i of ids) for (const j of near(l, i, l === 0 ? 3 : 2)) { m.get(i).add(j); m.get(j).add(i); }
    return m;
  });
  const at = (l, i) => [base[i][0], Y[l], base[i][1]];
  const linePos = [];
  const lineA = [];
  layers.forEach((ids, l) => {
    const done = new Set();
    for (const i of ids) for (const j of adj[l].get(i)) {
      const key = i < j ? `${i}-${j}` : `${j}-${i}`;
      if (done.has(key)) continue;
      done.add(key);
      linePos.push(...at(l, i), ...at(l, j));
      lineA.push(0.45, 0.45);
    }
    if (l) for (const i of ids) { linePos.push(...at(l, i), ...at(l - 1, i)); lineA.push(0.22, 0.22); }
    for (let k = 0; k < 64; k++) {
      const a0 = (k / 64) * Math.PI * 2;
      const a1 = ((k + 1) / 64) * Math.PI * 2;
      linePos.push(Math.cos(a0) * 3.3, Y[l], Math.sin(a0) * 3.3, Math.cos(a1) * 3.3, Y[l], Math.sin(a1) * 3.3);
      lineA.push(0.2, 0.2);
    }
  });
  materials.push(laneLines(U, geometry([["position", linePos, 3], ["aA", lineA, 1]]), 1, 1, group, [1, 0]).mat);
  const slots = [];
  layers.forEach((ids, l) => ids.forEach((i) => slots.push([l, i])));
  const slotOf = new Map(slots.map(([l, i], k) => [`${l}:${i}`, k]));
  const TRAIL = 10;
  const count = slots.length + 1 + TRAIL;
  const pos = new Float32Array(count * 3);
  const size = new Float32Array(count);
  const alpha = new Float32Array(count);
  const bright = new Float32Array(count);
  slots.forEach(([l, i], k) => { pos.set(at(l, i), k * 3); size[k] = l ? 0.2 : 0.15; alpha[k] = l ? 0.9 : 0.75; });
  const head = slots.length;
  const pts = lanePoints(U, [["position", pos, 3, true], ["aSize", size, 1, true], ["aA", alpha, 1, true], ["aB", bright, 1, true]], 3, group);
  materials.push(pts.mat);
  const descents = [];
  for (let n = 0; n < 4; n++) {
    const a = r() * Math.PI * 2;
    const d = 1 + r() * 1.8;
    const target = [Math.cos(a) * d, Math.sin(a) * d];
    const far = (i) => Math.hypot(base[i][0] - target[0], base[i][1] - target[1]);
    let cur = layers[2][n % layers[2].length];
    const path = [[2, cur]];
    for (let l = 2; l >= 0; l--) {
      if (l < 2) path.push([l, cur]);
      for (;;) {
        let next = cur;
        for (const j of adj[l].get(cur)) if (far(j) < far(next) - 1e-9) next = j;
        if (next === cur) break;
        cur = next;
        path.push([l, cur]);
      }
    }
    const points = path.map(([l, i]) => at(l, i));
    const lengths = [0];
    for (let k = 1; k < points.length; k++) lengths.push(lengths[k - 1] + dist3(points[k - 1], points[k]));
    descents.push({ path, points, lengths, total: Math.max(lengths[lengths.length - 1], 1e-6) });
  }
  const pathGeo = geometry([["position", new Float32Array(24 * 6), 3, true], ["aA", new Float32Array(24 * 2), 1, true], ["aB", new Float32Array(24 * 2).fill(1), 1]]);
  materials.push(laneLines(U, pathGeo, 1, 2, group).mat);
  const sample = (desc, u, out) => {
    const at2 = u * desc.total;
    let k = 1;
    while (k < desc.points.length - 1 && desc.lengths[k] < at2) k++;
    const seg = desc.lengths[k] - desc.lengths[k - 1];
    return mix3(desc.points[k - 1], desc.points[k], seg > 0 ? clamp((at2 - desc.lengths[k - 1]) / seg) : 1, out);
  };
  const tmpP = [0, 0, 0];
  const update = (t) => {
    group.rotation.y = t * 0.12;
    const n = Math.floor(t / 2.6);
    const T = t % 2.6;
    const desc = descents[n % descents.length];
    const u = clamp(T / 2.0);
    const fade = 1 - smooth((T - 2.15) / 0.45);
    const b = pts.geo.attributes.aB.array;
    const a = pts.geo.attributes.aA.array;
    const s = pts.geo.attributes.aSize.array;
    const p = pts.geo.attributes.position.array;
    b.fill(0, 0, slots.length);
    const reached = u * desc.total;
    desc.path.forEach(([l, i], k) => { if (desc.lengths[k] <= reached + 1e-6) b[slotOf.get(`${l}:${i}`)] = fade * (k === desc.path.length - 1 && u >= 1 ? 1 : 0.8); });
    p.set(sample(desc, u, tmpP), head * 3);
    s[head] = 0.3;
    a[head] = fade;
    b[head] = 1;
    for (let k = 0; k < TRAIL; k++) {
      const idx = head + 1 + k;
      const v = u - (k + 1) * 0.022;
      p.set(sample(desc, clamp(v), tmpP), idx * 3);
      s[idx] = 0.22 * (1 - k / TRAIL);
      a[idx] = v > 0 ? 0.7 * (1 - k / TRAIL) * fade : 0;
      b[idx] = 0.6;
    }
    const lp = pathGeo.attributes.position.array;
    const la = pathGeo.attributes.aA.array;
    let segs = 0;
    for (let k = 1; k < desc.points.length && segs < 24; k++) {
      if (desc.lengths[k - 1] >= reached) break;
      lp.set(desc.points[k - 1], segs * 6);
      const end = desc.lengths[k] <= reached ? desc.points[k] : sample(desc, u, tmpP);
      lp.set(end, segs * 6 + 3);
      la[segs * 2] = la[segs * 2 + 1] = 0.9 * fade;
      segs++;
    }
    pathGeo.setDrawRange(0, segs * 2);
    pathGeo.attributes.position.needsUpdate = true;
    pathGeo.attributes.aA.needsUpdate = true;
    for (const name of ["position", "aSize", "aA", "aB"]) pts.geo.attributes[name].needsUpdate = true;
  };
  return { U, scene, materials, update, still: 1.6, ex: 3.5, ey: 2.6, ez: 0.4, dir: [0, 2.6, 8.5] };
}

function storageLane() {
  const U = laneUniforms();
  const scene = new THREE.Scene();
  const group = add(scene, new THREE.Group(), 0);
  const materials = [];
  const boxGeo = new THREE.BoxGeometry(0.62, 0.9, 0.42);
  const edgeGeo = new THREE.EdgesGeometry(boxGeo);
  const boxes = [];
  for (let m = 0; m < 9; m++) {
    const face = material(SOLID_VERT, SOLID_FRAG, { uColor: { value: [1, 1, 1] }, uAlpha: { value: 0.3 } }, { side: THREE.FrontSide });
    face.userData.solid = true;
    const edge = material(FLAT_VERT, FLAT_FRAG, { uColor: { value: [1, 1, 1] }, uAlpha: { value: 0.9 } });
    const mesh = add(group, new THREE.Mesh(boxGeo, face), 1 + m * 0.01);
    const lines = add(group, new THREE.LineSegments(edgeGeo, edge), 2 + m * 0.01);
    materials.push(face, edge);
    boxes.push({ mesh, lines, face, edge });
  }
  const rail = [-3.7, -0.55, 0, 3.7, -0.55, 0];
  for (let m = 0; m < 9; m++) { const x = -3.2 + m * 0.8; rail.push(x, -0.55, 0, x, -0.64, 0); }
  materials.push(laneLines(U, geometry([["position", rail, 3]]), 0.5, 0, group, [1, 0]).mat);
  const scanGeo = geometry([["position", new Float32Array(6), 3, true], ["aA", [1, 1], 1, true], ["aB", [1, 1], 1]]);
  const scan = laneLines(U, scanGeo, 1, 3, group);
  materials.push(scan.mat);
  let colors = null;
  const update = (t) => {
    const u = (t % 2.4) / 2.4;
    const shift = -0.8 * smooth((u - 0.6) / 0.35);
    const lane = U.uCol.value;
    for (let m = 0; m < 9; m++) {
      const b = boxes[m];
      let y = 0;
      let alpha = 1;
      let color = lane;
      if (m === 8) {
        y = lerp(1.4, 0, easeOut(u / 0.25));
        alpha = clamp(u / 0.08);
        color = u < 0.25 ? colors.pending : u < 0.45 ? mix3(U.uHi.value, lane, smooth((u - 0.25) / 0.2), b.tmp || (b.tmp = [0, 0, 0])) : lane;
      }
      if (m === 0) alpha *= 1 - smooth((u - 0.6) / 0.35);
      const x = -3.2 + m * 0.8 + shift;
      b.mesh.position.set(x, y, 0);
      b.lines.position.set(x, y, 0);
      b.face.uniforms.uColor.value = color;
      b.edge.uniforms.uColor.value = color;
      b.face.uniforms.uAlpha.value = colors.face * alpha;
      b.edge.uniforms.uAlpha.value = 0.9 * alpha;
      b.mesh.visible = b.lines.visible = alpha > 0.01;
    }
    const sp = scanGeo.attributes.position.array;
    const su = (u - 0.25) / 0.2;
    const on = su > 0 && su < 1;
    const x = -3.2 + 8 * 0.8 + shift;
    const y = lerp(0.45, -0.45, clamp(su));
    sp.set([x - 0.33, y, 0.22, x + 0.33, y, 0.22]);
    scanGeo.attributes.aA.array.fill(on ? Math.sin(Math.PI * su) : 0);
    scanGeo.attributes.position.needsUpdate = true;
    scanGeo.attributes.aA.needsUpdate = true;
  };
  const theme = (P) => { colors = { pending: P.payload, face: P.dark ? 0.32 : 0.26 }; };
  return { U, scene, materials, update, theme, still: 1.2, ex: 3.6, ey: 1.2, ez: 0.4, dir: [0.35, 0.45, 1] };
}

function graphLane() {
  const U = laneUniforms();
  const r = rng(5);
  const scene = new THREE.Scene();
  const group = add(scene, new THREE.Group(), 0);
  group.rotation.x = 0.3;
  const materials = [];
  const N = 18;
  const nodes = [];
  for (let i = 0; i < N; i++) {
    const y = 1 - ((i + 0.5) / N) * 2;
    const rad = Math.sqrt(1 - y * y);
    const th = i * 2.39996;
    nodes.push([Math.cos(th) * rad * 1.7 + (r() - 0.5) * 0.3, y * 1.7 + (r() - 0.5) * 0.3, Math.sin(th) * rad * 1.7 + (r() - 0.5) * 0.3]);
  }
  const adj = nodes.map(() => new Set());
  const edges = [];
  const pairs = [];
  for (let a = 0; a < N; a++) for (let b = a + 1; b < N; b++) pairs.push([dist3(nodes[a], nodes[b]), a, b]);
  pairs.sort((x, y) => x[0] - y[0]);
  for (const [, a, b] of pairs) {
    if (edges.length >= 26) break;
    if (adj[a].size >= 4 || adj[b].size >= 4) continue;
    adj[a].add(b); adj[b].add(a); edges.push([a, b]);
  }
  const linePos = [];
  for (const [a, b] of edges) linePos.push(...nodes[a], ...nodes[b]);
  const lines = laneLines(U, geometry([["position", linePos, 3]]), 1, 1, group, [0.35, 0]);
  materials.push(lines.mat);
  const pts = lanePoints(U, [["position", nodes.flat(), 3], ["aSize", new Float32Array(N).fill(0.26), 1, true], ["aA", new Float32Array(N).fill(0.9), 1], ["aB", new Float32Array(N), 1, true]], 2, group);
  materials.push(pts.mat);
  const levels = (start) => {
    const level = new Array(N).fill(-1);
    const parent = new Array(N).fill(-1);
    level[start] = 0;
    const queue = [start];
    while (queue.length) { const a = queue.shift(); for (const b of adj[a]) if (level[b] < 0) { level[b] = level[a] + 1; parent[b] = a; queue.push(b); } }
    return { level, parent };
  };
  const bfs = Array.from({ length: N }, (_, i) => levels(i));
  const update = (t) => {
    group.rotation.y = t * 0.25;
    const n = Math.floor(t / 2.2);
    const T = t % 2.2;
    const { level, parent } = bfs[(n * 5) % N];
    const fade = 1 - smooth((T - 1.5) / 0.7);
    const glow = [smooth(T / 0.3), smooth((T - 0.3) / 0.6), smooth((T - 0.9) / 0.6)];
    const b = pts.geo.attributes.aB.array;
    const s = pts.geo.attributes.aSize.array;
    for (let i = 0; i < N; i++) {
      const v = level[i] >= 0 && level[i] <= 2 ? glow[level[i]] * fade : 0;
      b[i] = v;
      s[i] = level[i] === 0 ? 0.26 + 0.14 * v : 0.26;
    }
    const la = lines.geo.attributes.aA.array;
    const lb = lines.geo.attributes.aB.array;
    edges.forEach(([x, y], k) => {
      const child = parent[y] === x ? y : parent[x] === y ? x : -1;
      const v = child >= 0 ? b[child] : 0;
      la[k * 2] = la[k * 2 + 1] = 0.3 + 0.6 * v;
      lb[k * 2] = lb[k * 2 + 1] = v;
    });
    pts.geo.attributes.aB.needsUpdate = true;
    pts.geo.attributes.aSize.needsUpdate = true;
    lines.geo.attributes.aA.needsUpdate = true;
    lines.geo.attributes.aB.needsUpdate = true;
  };
  return { U, scene, materials, update, still: 1.1, ex: 2.2, ey: 2.0, ez: 0.4, dir: [0, 0.3, 1] };
}

const LANES = { "product-connect": sdkLane, "product-retrieve": searchLane, "product-store": storageLane, "product-graph": graphLane };

function watchLanes() {
  const core = document.querySelector("body.landing .product-map-core");
  if (!core || !("IntersectionObserver" in window)) return;
  const io = new IntersectionObserver((entries) => {
    if (!entries.some((entry) => entry.isIntersecting)) return;
    io.disconnect();
    if (!dead) createLanes(core);
  }, { rootMargin: "200px 0px" });
  io.observe(core);
  cleanups.push(() => io.disconnect());
}

function createLanes(core) {
  const canvas = document.createElement("canvas");
  canvas.className = "lane-canvas";
  canvas.setAttribute("aria-hidden", "true");
  let renderer = null;
  let disposed = false;
  const view = { fps: 30, optional: true, observers: [] };
  const dispose = () => {
    if (disposed) return;
    disposed = true;
    const index = themeHooks.indexOf(theme);
    if (index >= 0) themeHooks.splice(index, 1);
    if (renderer) renderer.dispose();
    canvas.remove();
    core.classList.remove("lanes-3d");
  };
  const lanes = [];
  const applyTheme = () => {
    const P = palette();
    const hi = rgb(P.dark ? "#f8fafc" : "#312e81");
    for (const lane of lanes) {
      lane.U.uCol.value = rgb(getComputedStyle(lane.row).getPropertyValue("--lane"), "#8b5cf6");
      lane.U.uHi.value = hi;
      lane.U.uSharp.value = P.dark ? 0 : 1;
      for (const m of lane.materials) m.blending = P.dark && !m.userData.solid ? THREE.AdditiveBlending : THREE.NormalBlending;
      if (lane.theme) lane.theme(P);
    }
  };
  const theme = () => {
    try { applyTheme(); } catch (error) {
      console.warn("VLite 3D cards turned off:", error);
      dropView(view);
    }
  };
  view.dispose = dispose;
  try {
    const gl = context(canvas);
    if (!gl) return;
    renderer = makeRenderer(canvas, gl);
    renderer.autoClear = false;
    for (const row of core.querySelectorAll(".product-map-row")) {
      const art = row.querySelector(".lane-art");
      const make = Object.keys(LANES).find((name) => row.classList.contains(name));
      if (!art || !make) continue;
      const lane = LANES[make]();
      lane.row = row;
      lane.art = art;
      lane.camera = new THREE.PerspectiveCamera(35, 1, 0.1, 100);
      lane.dir = new THREE.Vector3(...lane.dir).normalize();
      lanes.push(lane);
    }
    if (!lanes.length) { dispose(); return; }
    applyTheme();
    themeHooks.push(theme);
    canvas.addEventListener("webglcontextlost", (event) => { event.preventDefault(); dropView(view); });
    const cur = { W: 0, H: 0, pr: 0 };
    const tan = Math.tan((35 * DEG) / 2);
    view.render = (time) => {
      const W = Math.max(1, core.clientWidth);
      const H = Math.max(1, core.clientHeight);
      const pr = Math.min(devicePixelRatio || 1, 2);
      if (pr !== cur.pr) { cur.pr = pr; renderer.setPixelRatio(pr); }
      if (W !== cur.W || H !== cur.H) { cur.W = W; cur.H = H; renderer.setSize(W, H, false); }
      const boxes = lanes.map(({ row, art }) => [row.offsetLeft + row.clientLeft + art.offsetLeft, row.offsetTop + row.clientTop + art.offsetTop, art.offsetWidth, art.offsetHeight]);
      renderer.setScissorTest(false);
      renderer.setViewport(0, 0, W, H);
      renderer.clear();
      renderer.setScissorTest(true);
      lanes.forEach((lane, i) => {
        const [x, y, w, h] = boxes[i];
        if (w < 4 || h < 4) return;
        const gy = H - y - h;
        renderer.setViewport(x, gy, w, h);
        renderer.setScissor(x, gy, w, h);
        const cam = lane.camera;
        cam.aspect = w / h;
        cam.position.copy(lane.dir).multiplyScalar(Math.max(lane.ey / tan, lane.ex / (tan * cam.aspect)) * 1.08 + lane.ez);
        cam.lookAt(0, 0, 0);
        cam.updateProjectionMatrix();
        lane.U.uPx.value = pr;
        lane.U.uScale.value = h / (2 * tan);
        lane.update(time === null ? lane.still : time);
        renderer.render(lane.scene, cam);
      });
      renderer.setScissorTest(false);
    };
    core.append(canvas);
    core.classList.add("lanes-3d");
    addView(view, core);
    cleanups.push(() => dropView(view));
    schedule();
  } catch (error) {
    console.warn("VLite 3D cards turned off:", error);
    dispose();
  }
}

// ---------------------------------------------------------------------------
// Boot

async function boot() {
  const canvas = document.createElement("canvas");
  canvas.className = "hero-canvas";
  canvas.setAttribute("aria-hidden", "true");
  const gl = context(canvas);
  if (!gl) { teardown("no-webgl2"); return; }
  if (!FORCE && softwareGL(gl)) {
    const lose = gl.getExtension("WEBGL_lose_context");
    if (lose) lose.loseContext();
    teardown("software-gl");
    return;
  }
  THREE = await import("./vendor/three/three.min.js");
  if (dead) return;
  const renderer = makeRenderer(canvas, gl);
  cleanups.push(() => { renderer.dispose(); canvas.remove(); hero.classList.remove("has-3d"); });
  canvas.addEventListener("webglcontextlost", (event) => { event.preventDefault(); teardown("context-lost"); });
  const view = createHero(renderer);
  hero.prepend(canvas);
  addView(view, hero);
  cleanups.push(() => { for (const observer of view.observers) observer.disconnect(); });

  const onMotion = () => { for (const v of views) v.last = 0; markDirty(); };
  motionQuery.addEventListener("change", onMotion);
  cleanups.push(() => motionQuery.removeEventListener("change", onMotion));
  const onVisibility = () => { for (const v of views) v.last = 0; schedule(); };
  document.addEventListener("visibilitychange", onVisibility);
  cleanups.push(() => document.removeEventListener("visibilitychange", onVisibility));
  const themeObserver = new MutationObserver(() => {
    for (const hook of themeHooks.slice()) {
      try { hook(); } catch (error) { fail(error); return; }
    }
    markDirty();
  });
  themeObserver.observe(root, { attributes: true, attributeFilter: ["data-theme"] });
  cleanups.push(() => themeObserver.disconnect());
  if (finePointer.matches) {
    const onMove = (event) => {
      pointer.tx = clamp((event.clientX / innerWidth) * 2 - 1, -1, 1);
      pointer.ty = clamp((event.clientY / innerHeight) * 2 - 1, -1, 1);
    };
    addEventListener("pointermove", onMove, { passive: true });
    cleanups.push(() => removeEventListener("pointermove", onMove));
  }
  if (document.fonts && document.fonts.ready) document.fonts.ready.then(markDirty, () => {});

  view.render(motionQuery.matches ? null : 0);
  view.dirty = false;
  hero.classList.add("has-3d");
  setState("ready");
  watchLanes();
  schedule();
}

function start() {
  const connection = navigator.connection;
  if (connection && connection.saveData && !FORCE) { setState("off", "save-data"); return; }
  setState("pending");
  const go = () => { if (!dead) boot().catch(fail); };
  const idle = () => ("requestIdleCallback" in window ? requestIdleCallback(go, { timeout: 1500 }) : setTimeout(go, 200));
  if (document.readyState === "complete") idle();
  else addEventListener("load", idle, { once: true });
}

if (hero && map) start();
