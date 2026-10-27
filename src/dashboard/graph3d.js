let THREE;
export async function loadRenderer() {
  THREE ??= await import("./three.min.js");
}

// Bounded, deterministic spring/charge simulation. IDs stay strings throughout.
export function forceLayout(nodes, edges) {
  const points = nodes.map((node, index) => {
    const y = nodes.length === 1 ? 0 : 1 - 2 * (index + .5) / nodes.length;
    const ring = Math.sqrt(1 - y * y);
    const angle = index * Math.PI * (3 - Math.sqrt(5));
    return { ...node, x: Math.cos(angle) * ring * 100, y: y * 100, z: Math.sin(angle) * ring * 100 };
  });
  const index = new Map(points.map((node, i) => [node.id, i]));
  const pairs = new Map();
  for (const edge of edges) {
    const a = index.get(edge.source), b = index.get(edge.target);
    if (a !== undefined && b !== undefined && a !== b) pairs.set(`${Math.min(a, b)}:${Math.max(a, b)}`, [a, b]);
  }
  const links = [...pairs.values()];
  for (let step = 0; step < 240; step++) {
    const forces = points.map(node => [-node.x * .008, -node.y * .008, -node.z * .008]);
    for (let a = 0; a < points.length; a++) {
      for (let b = a + 1; b < points.length; b++) {
        const left = points[a], right = points[b];
        const delta = [left.x - right.x, left.y - right.y, left.z - right.z];
        const distance = Math.max(12, Math.hypot(...delta));
        const charge = 1300 / (distance * distance * distance);
        for (let axis = 0; axis < 3; axis++) {
          forces[a][axis] += delta[axis] * charge;
          forces[b][axis] -= delta[axis] * charge;
        }
      }
    }
    for (const [a, b] of links) {
      const left = points[a], right = points[b];
      const delta = [right.x - left.x, right.y - left.y, right.z - left.z];
      const distance = Math.max(1, Math.hypot(...delta));
      const spring = (distance - 65) * .014 / distance;
      for (let axis = 0; axis < 3; axis++) {
        forces[a][axis] += delta[axis] * spring;
        forces[b][axis] -= delta[axis] * spring;
      }
    }
    const cooling = 1.4 * (1 - step / 280);
    points.forEach((point, i) => {
      point.x += forces[i][0] * cooling;
      point.y += forces[i][1] * cooling;
      point.z += forces[i][2] * cooling;
    });
  }
  const extent = Math.max(1, ...points.map(point => Math.hypot(point.x, point.y, point.z)));
  const scale = points.length > 1 ? 135 / extent : 0;
  return points.map(point => ({ ...point, x: point.x * scale, y: point.y * scale, z: point.z * scale }));
}

function color(value) {
  const hex = value.trim().replace("#", "");
  return [0, 2, 4].map(offset => parseInt(hex.slice(offset, offset + 2), 16) / 255);
}

function distanceToSegment(x, y, a, b) {
  const dx = b.x - a.x, dy = b.y - a.y;
  const length = dx * dx + dy * dy;
  const t = length ? Math.max(0, Math.min(1, ((x - a.x) * dx + (y - a.y) * dy) / length)) : 0;
  return Math.hypot(x - a.x - t * dx, y - a.y - t * dy);
}

export function placeLabels(items, width, height) {
  const occupied = items.map(item => ({ left: item.x - 9, top: item.y - 9, width: 18, height: 18 }));
  return items.map(item => {
    const w = item.width, h = item.height;
    const candidates = [
      [item.x, item.y + 14], [item.x, item.y - h - 14],
      [item.x + w / 2 + 14, item.y - h / 2], [item.x - w / 2 - 14, item.y - h / 2],
      [item.x + w / 2, item.y + 14], [item.x - w / 2, item.y + 14],
      [item.x + w / 2, item.y - h - 14], [item.x - w / 2, item.y - h - 14],
    ].map(([x, y]) => ({ left: Math.max(4, Math.min(width - w - 4, x - w / 2)), top: Math.max(4, Math.min(height - h - 4, y)), width: w, height: h }));
    const score = box => occupied.reduce((total, other) => total +
      Math.max(0, Math.min(box.left + w + 4, other.left + other.width) - Math.max(box.left - 4, other.left)) *
      Math.max(0, Math.min(box.top + h + 4, other.top + other.height) - Math.max(box.top - 4, other.top)), 0);
    let best = candidates[0], bestScore = score(best);
    for (const candidate of candidates.slice(1)) {
      const overlap = score(candidate);
      if (overlap < bestScore) { best = candidate; bestScore = overlap; }
    }
    occupied.push(best);
    return best;
  });
}

export function createGraph(host, view, { onNode, onEdge, onFailure }) {
  if (navigator.connection?.saveData) throw new Error("Data-saving mode is on");
  const canvas = document.createElement("canvas");
  canvas.setAttribute("aria-hidden", "true");
  const context = canvas.getContext("webgl2", { alpha: true, antialias: true, powerPreference: "low-power" });
  if (!context) throw new Error("WebGL2 is unavailable");
  const renderer = new THREE.WebGLRenderer({ canvas, context, alpha: true, antialias: true, powerPreference: "low-power" });
  const scene = new THREE.Scene();
  const group = new THREE.Group();
  scene.add(group);
  const camera = new THREE.PerspectiveCamera(42, 1, 1, 2000);
  camera.position.z = 440;
  const nodes = forceLayout(view.nodes, view.edges);
  const byId = new Map(nodes.map(node => [node.id, node]));
  const labels = document.createElement("div");
  labels.className = "graph-three-labels";
  labels.style.visibility = "hidden";
  const nodeGeometry = new THREE.IcosahedronGeometry(7, 2);
  const nodeMaterial = new THREE.ShaderMaterial({
    uniforms: { tint: { value: [1, 1, 1] } },
    vertexShader: "varying vec3 vNormal; void main(){vNormal=normalize(normalMatrix*normal);gl_Position=projectionMatrix*modelViewMatrix*vec4(position,1.);}",
    fragmentShader: "uniform vec3 tint; varying vec3 vNormal; void main(){float light=.48+.52*max(dot(normalize(vNormal),normalize(vec3(-.4,.7,1.))),0.);gl_FragColor=vec4(tint*light,1.);}",
  });
  const labelNodes = nodes.map(node => {
    const mesh = new THREE.Mesh(nodeGeometry, nodeMaterial);
    mesh.position.set(node.x, node.y, node.z);
    group.add(mesh);
    const button = document.createElement("button");
    button.type = "button";
    button.className = "graph-three-node";
    button.textContent = node.text;
    button.dataset.nodeId = node.id;
    button.setAttribute("aria-label", `Select node ${node.text}`);
    button.title = `${node.text} (ID ${node.id})`;
    button.addEventListener("click", () => onNode(node.id));
    labels.append(button);
    return { node, button };
  });
  const edgePaths = view.edges.map(edge => {
    const from = byId.get(edge.source), to = byId.get(edge.target);
    if (edge.source !== edge.target) return { edge, points: [from, to] };
    const points = [];
    for (let i = 0; i <= 24; i++) {
      const angle = i * Math.PI * 2 / 24;
      points.push({ x: from.x + 18 * (1 - Math.cos(angle)), y: from.y + 18 * Math.sin(angle), z: from.z });
    }
    return { edge, points };
  });
  const edgeLabels = edgePaths.map(({ edge, points }) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "graph-three-edge";
    button.textContent = edge.label;
    button.dataset.edgeId = edge.id;
    button.setAttribute("aria-label", `Select edge ${edge.label} (ID ${edge.id})`);
    button.addEventListener("click", () => onEdge(edge.id));
    labels.prepend(button);
    const middle = Math.floor(points.length / 2);
    const a = points[middle - 1], b = points[middle];
    return { button, point: points.length > 2 ? b : { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2, z: (a.z + b.z) / 2 } };
  });
  const positions = [];
  for (const { points } of edgePaths) {
    for (let i = 1; i < points.length; i++) {
      for (const point of [points[i - 1], points[i]]) positions.push(point.x, point.y, point.z);
    }
    const from = points[points.length - 2], to = points[points.length - 1];
    const delta = new THREE.Vector3(to.x - from.x, to.y - from.y, to.z - from.z).normalize();
    const end = new THREE.Vector3(to.x, to.y, to.z).addScaledVector(delta, -9);
    let side = new THREE.Vector3(-delta.y, delta.x, 0);
    if (side.lengthSq() < .01) side = new THREE.Vector3(1, 0, 0);
    side.normalize();
    for (const sign of [-1, 1]) {
      const tail = end.clone().addScaledVector(delta, -8).addScaledVector(side, sign * 4);
      positions.push(end.x, end.y, end.z, tail.x, tail.y, tail.z);
    }
  }
  const edgeGeometry = new THREE.BufferGeometry();
  edgeGeometry.setAttribute("position", new THREE.BufferAttribute(new Float32Array(positions), 3));
  const edgeMaterial = new THREE.ShaderMaterial({
    transparent: true,
    uniforms: { tint: { value: [1, 1, 1] } },
    vertexShader: "void main(){gl_Position=projectionMatrix*modelViewMatrix*vec4(position,1.);}",
    fragmentShader: "uniform vec3 tint;void main(){gl_FragColor=vec4(tint,.65);}",
  });
  group.add(new THREE.LineSegments(edgeGeometry, edgeMaterial));
  const pulseGeometry = new THREE.BufferGeometry();
  pulseGeometry.setAttribute("position", new THREE.BufferAttribute(new Float32Array(edgePaths.length * 3), 3));
  const pulseMaterial = new THREE.ShaderMaterial({
    transparent: true,
    depthWrite: false,
    uniforms: { tint: { value: [1, 1, 1] }, ratio: { value: 1 } },
    vertexShader: "uniform float ratio;void main(){gl_Position=projectionMatrix*modelViewMatrix*vec4(position,1.);gl_PointSize=7.*ratio;}",
    fragmentShader: "uniform vec3 tint;void main(){float d=length(gl_PointCoord-.5);if(d>.5)discard;gl_FragColor=vec4(tint,1.-d*2.);}",
  });
  const pulses = new THREE.Points(pulseGeometry, pulseMaterial);
  pulses.frustumCulled = false;
  group.add(pulses);
  host.replaceChildren(canvas, labels);
  host.hidden = false;
  host.dataset.renderer = "three";
  host.dataset.frames = "0";
  host.dataset.paused = "true";
  const motion = matchMedia("(prefers-reduced-motion: reduce)");
  let visible = false, disposed = false, frame = 0, last = 0, renders = 0;
  let width = 1, height = 1, labelsDirty = true;
  const vector = new THREE.Vector3();
  const project = point => {
    vector.set(point.x, point.y, point.z).applyMatrix4(group.matrixWorld).project(camera);
    return { x: (vector.x + 1) * width / 2, y: (1 - vector.y) * height / 2 };
  };
  const paint = (time = 0) => {
    frame = 0;
    if (disposed || !visible || document.hidden) return;
    if (time - last >= 1000 / 24 || !last) {
      last = time;
      const buffer = pulseGeometry.attributes.position;
      edgePaths.forEach(({ points }, index) => {
        const t = (motion.matches ? .35 : (time / 3500 + index * .17) % 1) * (points.length - 1);
        const at = Math.min(points.length - 2, Math.floor(t)), fraction = t - at;
        const from = points[at], to = points[at + 1];
        buffer.setXYZ(index, from.x + (to.x - from.x) * fraction, from.y + (to.y - from.y) * fraction, from.z + (to.z - from.z) * fraction);
      });
      buffer.needsUpdate = true;
      renderer.render(scene, camera);
      if (labelsDirty) {
        const projected = labelNodes.map(({ node, button }) => ({
          ...project(node), width: button.offsetWidth, height: button.offsetHeight,
        }));
        const placed = placeLabels(projected, width, height);
        labelNodes.forEach(({ button }, index) => {
          button.style.left = `${placed[index].left + placed[index].width / 2}px`;
          button.style.top = `${placed[index].top}px`;
          button.dataset.pickX = String(projected[index].x);
          button.dataset.pickY = String(projected[index].y);
        });
        edgeLabels.forEach(({ point, button }) => {
          const at = project(point);
          button.style.left = `${at.x}px`;
          button.style.top = `${at.y - 20}px`;
        });
        labels.style.visibility = "";
        labelsDirty = false;
      }
      host.dataset.frames = String(++renders);
    }
    if (!motion.matches && edgePaths.length) frame = requestAnimationFrame(paint);
  };
  const requestPaint = () => {
    last = 0;
    if (!frame && !disposed && visible && !document.hidden) frame = requestAnimationFrame(paint);
  };
  const recolor = () => {
    const style = getComputedStyle(document.documentElement);
    nodeMaterial.uniforms.tint.value = color(style.getPropertyValue("--vlite-accent"));
    edgeMaterial.uniforms.tint.value = color(style.getPropertyValue("--vlite-muted"));
    pulseMaterial.uniforms.tint.value = color(style.getPropertyValue("--vlite-search"));
    requestPaint();
  };
  const resize = () => {
    width = Math.max(1, host.clientWidth); height = Math.max(1, host.clientHeight);
    const ratio = Math.min(devicePixelRatio || 1, 1.5);
    renderer.setPixelRatio(ratio);
    renderer.setSize(width, height, false);
    pulseMaterial.uniforms.ratio.value = ratio;
    camera.aspect = width / height;
    camera.position.z = Math.max(440, 350 / camera.aspect);
    camera.updateProjectionMatrix();
    labelsDirty = true;
    requestPaint();
  };
  const theme = new MutationObserver(recolor);
  theme.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  const size = new ResizeObserver(resize);
  size.observe(host);
  const intersection = new IntersectionObserver(entries => {
    visible = entries[0].isIntersecting;
    host.dataset.paused = String(!visible);
    if (!visible) { cancelAnimationFrame(frame); frame = 0; } else { resize(); requestPaint(); }
  });
  intersection.observe(host);
  canvas.addEventListener("click", event => {
    const rect = canvas.getBoundingClientRect();
    const x = event.clientX - rect.left, y = event.clientY - rect.top;
    let bestNode, bestDistance = 16;
    for (const node of nodes) {
      const at = project(node), distance = Math.hypot(x - at.x, y - at.y);
      if (distance < bestDistance) { bestNode = node; bestDistance = distance; }
    }
    if (bestNode) { onNode(bestNode.id); return; }
    let bestEdge;
    bestDistance = 9;
    for (const { edge, points } of edgePaths) {
      for (let i = 1; i < points.length; i++) {
        const distance = distanceToSegment(x, y, project(points[i - 1]), project(points[i]));
        if (distance < bestDistance) { bestEdge = edge; bestDistance = distance; }
      }
    }
    if (bestEdge) onEdge(bestEdge.id);
  });
  const visibility = () => {
    if (document.hidden) { cancelAnimationFrame(frame); frame = 0; } else requestPaint();
  };
  const fonts = () => { labelsDirty = true; requestPaint(); };
  motion.addEventListener("change", requestPaint);
  document.fonts.addEventListener("loadingdone", fonts);
  document.addEventListener("visibilitychange", visibility);
  canvas.addEventListener("webglcontextlost", event => {
    event.preventDefault();
    if (!disposed) onFailure("The graphics context was lost");
  });
  recolor();
  resize();
  return {
    rotate(yaw, pitch) {
      group.rotation.set(Number(pitch) * Math.PI / 180, Number(yaw) * Math.PI / 180, 0);
      labelsDirty = true;
      requestPaint();
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      cancelAnimationFrame(frame);
      intersection.disconnect(); size.disconnect(); theme.disconnect();
      motion.removeEventListener("change", requestPaint);
      document.fonts.removeEventListener("loadingdone", fonts);
      document.removeEventListener("visibilitychange", visibility);
      nodeGeometry.dispose(); nodeMaterial.dispose(); edgeGeometry.dispose(); edgeMaterial.dispose();
      pulseGeometry.dispose(); pulseMaterial.dispose();
      renderer.dispose();
      renderer.forceContextLoss();
      host.replaceChildren();
      host.hidden = true;
      delete host.dataset.renderer;
      delete host.dataset.frames;
      delete host.dataset.paused;
    },
  };
}
