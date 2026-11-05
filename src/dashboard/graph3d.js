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

export function createGraph(host, view, { onNode, onEdge, onFailure, onChange = () => {} }) {
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
  const motion = matchMedia("(prefers-reduced-motion: reduce)");
  const nodes = forceLayout(view.nodes, view.edges).map((node, index, all) => {
    const y = all.length === 1 ? 0 : 1 - 2 * (index + .5) / all.length;
    const ring = Math.sqrt(1 - y * y), angle = index * Math.PI * (3 - Math.sqrt(5));
    const target = { x: node.x, y: node.y, z: node.z };
    const start = { x: Math.cos(angle) * ring * 100, y: y * 100, z: Math.sin(angle) * ring * 100 };
    return { ...node, startX: start.x, startY: start.y, startZ: start.z,
      targetX: target.x, targetY: target.y, targetZ: target.z,
      x: motion.matches ? target.x : start.x, y: motion.matches ? target.y : start.y, z: motion.matches ? target.z : start.z };
  });
  const byId = new Map(nodes.map(node => [node.id, node]));
  const nodeMeshes = new Map();
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
    nodeMeshes.set(node.id, mesh);
    const button = document.createElement("button");
    button.type = "button";
    button.className = "graph-three-node";
    button.textContent = node.text;
    button.dataset.nodeId = node.id;
    button.setAttribute("aria-label", `Select node ${node.text}`);
    button.title = `${node.text} (ID ${node.id})`;
    button.addEventListener("click", () => onNode(node.id));
    labels.append(button);
    return { node, mesh, button };
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
    return { edge, button, point: points.length > 2 ? b : { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2, z: (a.z + b.z) / 2 } };
  });
  const edgeGeometry = new THREE.BufferGeometry();
  const edgeMaterial = new THREE.ShaderMaterial({
    transparent: true,
    vertexColors: true,
    vertexShader: "varying vec3 vColor;void main(){vColor=color;gl_Position=projectionMatrix*modelViewMatrix*vec4(position,1.);}",
    fragmentShader: "varying vec3 vColor;void main(){gl_FragColor=vec4(vColor,.8);}",
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
  host.dataset.settled = "false";
  let visible = false, disposed = false, frame = 0, last = 0, renders = 0;
  let animationStarted = null;
  const settleDuration = 900, animationDuration = 3000;
  let width = 1, height = 1, labelsDirty = true, userZoomed = false, suppressPick = false;
  let selection = null, pointer = null;
  const vector = new THREE.Vector3();
  const intersectionPoint = new THREE.Vector3();
  const project = point => {
    vector.set(point.x, point.y, point.z).applyMatrix4(group.matrixWorld).project(camera);
    return { x: (vector.x + 1) * width / 2, y: (1 - vector.y) * height / 2 };
  };
  const updateEdgeGeometry = () => {
    const positions = [], colors = [];
    const style = getComputedStyle(document.documentElement);
    const muted = color(style.getPropertyValue("--vlite-muted"));
    const accent = color(style.getPropertyValue("--vlite-accent"));
    const search = color(style.getPropertyValue("--vlite-search"));
    for (const { edge, points } of edgePaths) {
      const active = selection?.kind === "edge" && selection.id === edge.id;
      const adjacent = selection?.kind === "node" && (edge.source === selection.id || edge.target === selection.id);
      const tint = active ? search : adjacent ? accent : selection ? muted.map(channel => channel * .38) : muted;
      for (let i = 1; i < points.length; i++) {
        for (const point of [points[i - 1], points[i]]) { positions.push(point.x, point.y, point.z); colors.push(...tint); }
      }
      const from = points[points.length - 2], to = points[points.length - 1];
      const delta = new THREE.Vector3(to.x - from.x, to.y - from.y, to.z - from.z).normalize();
      const end = new THREE.Vector3(to.x, to.y, to.z).addScaledVector(delta, -9);
      let side = new THREE.Vector3(-delta.y, delta.x, 0);
      if (side.lengthSq() < .01) side = new THREE.Vector3(1, 0, 0);
      side.normalize();
      for (const sign of [-1, 1]) {
        const tail = end.clone().addScaledVector(delta, -8).addScaledVector(side, sign * 4);
        for (const point of [end, tail]) { positions.push(point.x, point.y, point.z); colors.push(...tint); }
      }
    }
    edgeGeometry.setAttribute("position", new THREE.BufferAttribute(new Float32Array(positions), 3));
    edgeGeometry.setAttribute("color", new THREE.BufferAttribute(new Float32Array(colors), 3));
    if (positions.length) edgeGeometry.computeBoundingSphere();
    labels.querySelectorAll(".graph-three-edge").forEach(button => {
      const selected = selection?.kind === "edge" && selection.id === button.dataset.edgeId;
      button.classList.toggle("is-selected", !!selected);
      button.classList.toggle("is-neighbor", selection?.kind === "node" && edgePaths.some(({ edge }) => edge.id === button.dataset.edgeId && (edge.source === selection.id || edge.target === selection.id)));
      button.classList.toggle("is-dimmed", !!selection && !selected && !button.classList.contains("is-neighbor"));
    });
  };
  const updateNodeHighlight = () => {
    const neighbors = new Set();
    if (selection?.kind === "node") {
      for (const { edge } of edgePaths) {
        if (edge.source === selection.id) neighbors.add(edge.target);
        if (edge.target === selection.id) neighbors.add(edge.source);
      }
    } else if (selection?.kind === "edge") {
      const edge = edgePaths.find(item => item.edge.id === selection.id)?.edge;
      if (edge) { neighbors.add(edge.source); neighbors.add(edge.target); }
    }
    for (const { node, mesh, button } of labelNodes) {
      const selected = selection?.kind === "node" && selection.id === node.id;
      const neighbor = neighbors.has(node.id) && !selected;
      const dimmed = !!selection && !selected && !neighbor;
      mesh.scale.setScalar(selected ? 1.55 : neighbor ? 1.25 : dimmed ? .82 : 1);
      button.classList.toggle("is-selected", !!selected);
      button.classList.toggle("is-neighbor", neighbor);
      button.classList.toggle("is-dimmed", dimmed);
    }
    updateEdgeGeometry();
    requestPaint();
  };
  const paint = (time = 0) => {
    frame = 0;
    if (disposed || !visible || document.hidden) return;
    host.dataset.settled = String(motion.matches || (animationStarted !== null && time - animationStarted >= animationDuration));
    if (time - last >= 1000 / 24 || !last) {
      last = time;
      const elapsed = animationStarted === null ? 0 : time - animationStarted;
      const progress = motion.matches ? 1 : Math.min(1, Math.max(0, elapsed / settleDuration));
      const eased = 1 - (1 - progress) ** 3;
      if (progress < 1 || nodes.some(node => !node.settled)) {
        for (const node of nodes) {
          if (node.dragged) continue;
          node.x = node.startX + (node.targetX - node.startX) * eased;
          node.y = node.startY + (node.targetY - node.startY) * eased;
          node.z = node.startZ + (node.targetZ - node.startZ) * eased;
          node.settled = progress >= 1;
          nodeMeshes.get(node.id).position.set(node.x, node.y, node.z);
        }
        for (const { edge, points } of edgePaths) {
          if (edge.source !== edge.target) continue;
          const node = byId.get(edge.source);
          points.forEach((point, index) => {
            const angle = index * Math.PI * 2 / 24;
            point.x = node.x + 18 * (1 - Math.cos(angle));
            point.y = node.y + 18 * Math.sin(angle);
            point.z = node.z;
          });
        }
        for (const label of edgeLabels) {
          const points = edgePaths.find(path => path.edge.id === label.edge.id)?.points;
          if (!points) continue;
          const middle = Math.floor(points.length / 2), a = points[middle - 1], b = points[middle];
          label.point = points.length > 2 ? b : { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2, z: (a.z + b.z) / 2 };
        }
        updateEdgeGeometry();
        labelsDirty = true;
      }
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
        labelNodes.forEach(({ node, button }, index) => {
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
    if (!motion.matches && (edgePaths.length || nodes.length) && animationStarted !== null && time - animationStarted < animationDuration) frame = requestAnimationFrame(paint);
  };
  const requestPaint = () => {
    last = 0;
    if (!frame && !disposed && visible && !document.hidden) frame = requestAnimationFrame(paint);
  };
  const recolor = () => {
    const style = getComputedStyle(document.documentElement);
    nodeMaterial.uniforms.tint.value = color(style.getPropertyValue("--vlite-accent"));
    pulseMaterial.uniforms.tint.value = color(style.getPropertyValue("--vlite-search"));
    updateEdgeGeometry();
    requestPaint();
  };
  const resize = () => {
    width = Math.max(1, host.clientWidth); height = Math.max(1, host.clientHeight);
    const ratio = Math.min(devicePixelRatio || 1, 1.5);
    renderer.setPixelRatio(ratio);
    renderer.setSize(width, height, false);
    pulseMaterial.uniforms.ratio.value = ratio;
    camera.aspect = width / height;
    if (!userZoomed) camera.position.z = Math.max(440, 350 / camera.aspect);
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
    if (!visible) { cancelAnimationFrame(frame); frame = 0; }
    else { if (animationStarted === null) animationStarted = performance.now(); resize(); requestPaint(); }
  });
  intersection.observe(host);
  const nodeFromEvent = event => {
    const rect = canvas.getBoundingClientRect();
    const x = event.clientX - rect.left, y = event.clientY - rect.top;
    let hit = null, nearest = 16;
    for (const node of nodes) {
      const at = project(node), distance = Math.hypot(x - at.x, y - at.y);
      if (distance < nearest) { hit = node; nearest = distance; }
    }
    return hit;
  };
  const pointerDown = event => {
    if (event.button !== 0 || event.target !== canvas) return;
    const node = nodeFromEvent(event);
    if (node) {
      node.dragged = true;
      node.targetX = node.x; node.targetY = node.y; node.targetZ = node.z;
      group.updateMatrixWorld(true);
      const world = new THREE.Vector3(node.x, node.y, node.z).applyMatrix4(group.matrixWorld);
      pointer = { kind: "node", id: node.id, depth: world.z, startX: event.clientX, startY: event.clientY, moved: false };
    } else {
      pointer = { kind: "rotate", startX: event.clientX, startY: event.clientY, lastX: event.clientX, lastY: event.clientY, moved: false };
    }
    canvas.setPointerCapture(event.pointerId);
    canvas.classList.add("is-gesturing");
  };
  const pointerMove = event => {
    if (!pointer) return;
    const dx = event.clientX - pointer.lastX, dy = event.clientY - pointer.lastY;
    if (Math.hypot(event.clientX - pointer.startX, event.clientY - pointer.startY) > 3) pointer.moved = true;
    if (pointer.kind === "rotate") {
      group.rotation.y += dx * .008;
      group.rotation.x = Math.max(-1.35, Math.min(1.35, group.rotation.x + dy * .008));
      onChange(Math.round(group.rotation.y * 180 / Math.PI), Math.round(group.rotation.x * 180 / Math.PI));
    } else {
      const rect = canvas.getBoundingClientRect();
      intersectionPoint.set(((event.clientX - rect.left) / rect.width) * 2 - 1,
        -((event.clientY - rect.top) / rect.height) * 2 + 1, .5).unproject(camera).sub(camera.position).normalize();
      if (Math.abs(intersectionPoint.z) > .0001) {
        intersectionPoint.multiplyScalar((pointer.depth - camera.position.z) / intersectionPoint.z).add(camera.position);
        const local = group.worldToLocal(intersectionPoint.clone());
        const node = byId.get(pointer.id);
        node.x = local.x; node.y = local.y; node.z = local.z;
        nodeMeshes.get(pointer.id).position.copy(local);
        edgePaths.forEach(path => {
          if (path.edge.source !== pointer.id && path.edge.target !== pointer.id) return;
          if (path.edge.source === path.edge.target) {
            path.points.forEach((point, index) => {
              const angle = index * Math.PI * 2 / 24;
              point.x = node.x + 18 * (1 - Math.cos(angle));
              point.y = node.y + 18 * Math.sin(angle);
              point.z = node.z;
            });
          } else path.points = [byId.get(path.edge.source), byId.get(path.edge.target)];
        });
        edgeLabels.forEach(label => {
          const path = edgePaths.find(item => item.edge.id === label.edge.id);
          if (path && (path.edge.source === pointer.id || path.edge.target === pointer.id)) {
            const middle = Math.floor(path.points.length / 2), a = path.points[middle - 1], b = path.points[middle];
            label.point = path.points.length > 2 ? b : { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2, z: (a.z + b.z) / 2 };
          }
        });
        updateEdgeGeometry();
        labelsDirty = true;
      }
    }
    pointer.lastX = event.clientX; pointer.lastY = event.clientY;
    labelsDirty = true;
    requestPaint();
  };
  const pointerEnd = event => {
    if (!pointer) return;
    suppressPick = pointer.moved;
    if (suppressPick) requestAnimationFrame(() => { suppressPick = false; });
    pointer = null;
    canvas.classList.remove("is-gesturing");
    if (canvas.hasPointerCapture(event.pointerId)) canvas.releasePointerCapture(event.pointerId);
  };
  canvas.addEventListener("pointerdown", pointerDown);
  canvas.addEventListener("pointermove", pointerMove);
  canvas.addEventListener("pointerup", pointerEnd);
  canvas.addEventListener("pointercancel", pointerEnd);
  canvas.addEventListener("wheel", event => {
    if (!event.ctrlKey && !event.metaKey) return;
    event.preventDefault();
    userZoomed = true;
    camera.position.z = Math.max(180, Math.min(900, camera.position.z * (event.deltaY < 0 ? .9 : 1.1)));
    labelsDirty = true;
    requestPaint();
  }, { passive: false });
  canvas.addEventListener("click", event => {
    if (suppressPick) { event.preventDefault(); suppressPick = false; return; }
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
  canvas.addEventListener("webglcontextlost", event => {
    event.preventDefault();
    if (!disposed) onFailure("The graphics context was lost");
  });
  const visibility = () => {
    if (document.hidden) { cancelAnimationFrame(frame); frame = 0; } else requestPaint();
  };
  const fonts = () => { labelsDirty = true; requestPaint(); };
  motion.addEventListener("change", requestPaint);
  document.fonts.addEventListener("loadingdone", fonts);
  document.addEventListener("visibilitychange", visibility);
  recolor();
  resize();
  return {
    rotate(yaw, pitch) {
      group.rotation.set(Number(pitch) * Math.PI / 180, Number(yaw) * Math.PI / 180, 0);
      labelsDirty = true;
      requestPaint();
    },
    zoomBy(factor) {
      userZoomed = true;
      camera.position.z = Math.max(180, Math.min(900, camera.position.z / factor));
      labelsDirty = true;
      requestPaint();
    },
    fit() {
      const extent = Math.max(1, ...nodes.map(node => Math.hypot(node.targetX, node.targetY, node.targetZ))) + 24;
      const vertical = Math.tan(camera.fov * Math.PI / 360);
      camera.position.z = Math.max(220, extent / Math.min(vertical, vertical * camera.aspect) * 1.12);
      userZoomed = true;
      labelsDirty = true;
      requestPaint();
    },
    setSelection(value) {
      selection = value && { kind: value.kind, id: String(value.id) };
      updateNodeHighlight();
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      cancelAnimationFrame(frame);
      intersection.disconnect(); size.disconnect(); theme.disconnect();
      motion.removeEventListener("change", requestPaint);
      document.fonts.removeEventListener("loadingdone", fonts);
      document.removeEventListener("visibilitychange", visibility);
      canvas.removeEventListener("pointerdown", pointerDown);
      canvas.removeEventListener("pointermove", pointerMove);
      canvas.removeEventListener("pointerup", pointerEnd);
      canvas.removeEventListener("pointercancel", pointerEnd);
      nodeGeometry.dispose(); nodeMaterial.dispose(); edgeGeometry.dispose(); edgeMaterial.dispose();
      pulseGeometry.dispose(); pulseMaterial.dispose();
      renderer.dispose();
      renderer.forceContextLoss();
      host.replaceChildren();
      host.hidden = true;
      delete host.dataset.renderer;
      delete host.dataset.frames;
      delete host.dataset.paused;
      delete host.dataset.settled;
    },
  };
}
