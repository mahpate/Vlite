// One small, decorative renderer shared by sign-in and Overview.
async function boot() {
  const targets = [document.querySelector(".gate-brand"), document.querySelector("#database .section-heading")].filter(Boolean);
  if (!targets.length || navigator.connection?.saveData) return;
  const mounts = targets.map(target => {
    const mount = document.createElement("span");
    mount.className = "studio-ambience";
    mount.setAttribute("aria-hidden", "true");
    target.append(mount);
    return mount;
  });
  const canvas = document.createElement("canvas");
  canvas.setAttribute("aria-hidden", "true");
  const context = canvas.getContext("webgl2", { alpha: true, antialias: true, powerPreference: "low-power" });
  if (!context) { mounts.forEach(mount => mount.remove()); return; }
  const THREE = await import("./three.min.js");
  const renderer = new THREE.WebGLRenderer({ canvas, context, alpha: true, antialias: true });
  renderer.setPixelRatio(Math.min(devicePixelRatio || 1, 1.5));
  renderer.setSize(96, 56, false);
  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(36, 96 / 56, .1, 100);
  camera.position.z = 9;
  const group = new THREE.Group();
  scene.add(group);
  const geometry = new THREE.BoxGeometry(2.8, .22, 1.7);
  const edgeGeometry = new THREE.EdgesGeometry(geometry);
  const materials = [];
  for (let i = 0; i < 3; i++) {
    const material = new THREE.ShaderMaterial({
      transparent: true,
      uniforms: { tint: { value: [1, 1, 1] }, opacity: { value: .22 } },
      vertexShader: "varying vec3 vNormal;void main(){vNormal=normalMatrix*normal;gl_Position=projectionMatrix*modelViewMatrix*vec4(position,1.);}",
      fragmentShader: "uniform vec3 tint;uniform float opacity;varying vec3 vNormal;void main(){float light=.65+.35*abs(vNormal.z)/max(length(vNormal),.001);gl_FragColor=vec4(tint*light,opacity);}",
    });
    const outline = material.clone();
    outline.uniforms.opacity.value = .8;
    materials.push(material, outline);
    const layer = new THREE.Group();
    layer.position.y = (i - 1) * .75;
    layer.add(new THREE.Mesh(geometry, material), new THREE.LineSegments(edgeGeometry, outline));
    group.add(layer);
  }
  const motion = matchMedia("(prefers-reduced-motion: reduce)");
  const shown = new Set();
  let frame = 0, last = 0, disposed = false;
  const paint = (time = 0) => {
    frame = 0;
    if (disposed || document.hidden || !shown.size) return;
    const target = mounts.find(mount => shown.has(mount));
    if (canvas.parentNode !== target) target.append(canvas);
    if (!last || time - last >= 1000 / 20) {
      last = time;
      group.rotation.set(.35, motion.matches ? -.4 : -.4 + Math.sin(time / 6500) * .12, -.15);
      renderer.render(scene, camera);
    }
    if (!motion.matches) frame = requestAnimationFrame(paint);
  };
  const requestPaint = () => {
    last = 0;
    if (!frame && !disposed && shown.size && !document.hidden) frame = requestAnimationFrame(paint);
  };
  const recolor = () => {
    const style = getComputedStyle(document.documentElement);
    ["--vlite-accent", "--vlite-search", "--vlite-success"].forEach((token, index) => {
      const hex = style.getPropertyValue(token).trim().slice(1);
      const value = [0, 2, 4].map(offset => parseInt(hex.slice(offset, offset + 2), 16) / 255);
      materials[index * 2].uniforms.tint.value = value;
      materials[index * 2 + 1].uniforms.tint.value = value;
    });
    requestPaint();
  };
  const theme = new MutationObserver(recolor);
  theme.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  const observer = new IntersectionObserver(entries => {
    entries.forEach(entry => {
      if (entry.isIntersecting) shown.add(entry.target); else shown.delete(entry.target);
    });
    if (!shown.size) { cancelAnimationFrame(frame); frame = 0; } else requestPaint();
  });
  mounts.forEach(mount => observer.observe(mount));
  const visibility = () => {
    if (document.hidden) { cancelAnimationFrame(frame); frame = 0; } else requestPaint();
  };
  document.addEventListener("visibilitychange", visibility);
  motion.addEventListener("change", requestPaint);
  canvas.addEventListener("webglcontextlost", event => {
    event.preventDefault();
    cleanup();
  });
  function cleanup() {
    if (disposed) return;
    disposed = true;
    cancelAnimationFrame(frame);
    observer.disconnect(); theme.disconnect();
    document.removeEventListener("visibilitychange", visibility);
    motion.removeEventListener("change", requestPaint);
    geometry.dispose(); edgeGeometry.dispose(); materials.forEach(material => material.dispose());
    renderer.dispose();
    renderer.forceContextLoss();
    mounts.forEach(mount => mount.remove());
  }
  window.addEventListener("pagehide", cleanup, { once: true });
  recolor();
}

const start = () => boot().catch(error => {
  document.querySelectorAll(".studio-ambience").forEach(mount => mount.remove());
  console.warn("VLite decorative 3D is unavailable:", error.message);
});
if ("requestIdleCallback" in window) requestIdleCallback(start, { timeout: 2000 });
else setTimeout(start, 0);
window.addEventListener("pageshow", event => { if (event.persisted) start(); });
