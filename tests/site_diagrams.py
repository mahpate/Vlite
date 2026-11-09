"""Check shared diagram assets, theme switching, narrow layouts and the landing 3D view.

Usage: python tests/site_diagrams.py <assembled-site> [screenshot-directory]
Requires tests/requirements-dashboard.txt.
"""

import base64
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import sys
from threading import Thread
from urllib.parse import urlsplit
import xml.etree.ElementTree as ET

from playwright.sync_api import sync_playwright


ROOT = Path(__file__).resolve().parents[1]
SWIFTSHADER = ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"]
HERO_SETTLED = "['ready', 'off'].includes(document.querySelector('.hero-split').getAttribute('data-3d'))"
HERO_STATE = "(h => [h.getAttribute('data-3d'), h.getAttribute('data-3d-reason')])(document.querySelector('.hero-split'))"
HERO_KINDS = {
    "collections": "Collections", "dense": "Dense vectors", "sparse": "Sparse vectors",
    "meta": "Typed metadata", "payload": "Payloads", "graph": "Property graphs",
    "kv": "Key-value", "memory": "MemoryAI",
}
RENDERER = """(() => {
    const gl = document.createElement('canvas').getContext('webgl2');
    const info = gl && gl.getExtension('WEBGL_debug_renderer_info');
    return info ? String(gl.getParameter(info.UNMASKED_RENDERER_WEBGL)) : '';
})()"""
# Tethers rewrite each chip edge so it follows its cluster; compare against the served markup.
EDGES = """async () => {
    const source = new DOMParser().parseFromString(await (await fetch(location.pathname)).text(), 'text/html');
    const before = [...source.querySelectorAll('svg.file-map .fm-edge')].map(e => e.getAttribute('d'));
    const after = [...document.querySelectorAll('svg.file-map .fm-edge')].map(e => e.getAttribute('d'));
    return [before.length, after.length, before.filter((d, i) => d !== after[i]).length];
}"""
CHANGED = """async (shots) => {
    const pixels = await Promise.all(shots.map(async data => {
        const image = new Image();
        image.src = 'data:image/png;base64,' + data;
        await image.decode();
        const canvas = new OffscreenCanvas(image.width, image.height);
        const context = canvas.getContext('2d');
        context.drawImage(image, 0, 0);
        return context.getImageData(0, 0, image.width, image.height).data;
    }));
    const [a, b] = pixels;
    let changed = 0;
    for (let i = 0; i < a.length; i += 4) {
        if (Math.abs(a[i] - b[i]) + Math.abs(a[i + 1] - b[i + 1]) + Math.abs(a[i + 2] - b[i + 2]) > 48) changed++;
    }
    return changed / (a.length / 4);
}"""


class QuietHandler(SimpleHTTPRequestHandler):
    # Module scripts need a JavaScript MIME type; some platforms map .js to text/plain.
    extensions_map = {**SimpleHTTPRequestHandler.extensions_map,
                      ".js": "text/javascript", ".mjs": "text/javascript", ".woff2": "font/woff2"}

    def log_message(self, *_):
        pass


def track_errors(page, errors):
    page.on("pageerror", lambda error: errors.append(f"{page.url}: {error}"))
    page.on("console", lambda message: message.type == "error" and errors.append(f"{page.url}: {message.text}"))


def hero_state(page):
    page.wait_for_function(HERO_SETTLED, timeout=20000)
    return page.evaluate(HERO_STATE)


def changed_pixels(page, element, canvases):
    """Fraction of an element's pixels that change when the 3D canvases are hidden."""
    shown = element.screenshot()
    page.evaluate("s => document.querySelectorAll(s).forEach(c => { c.style.visibility = 'hidden'; })", canvases)
    hidden = element.screenshot()
    page.evaluate("s => document.querySelectorAll(s).forEach(c => { c.style.visibility = ''; })", canvases)
    return page.evaluate(CHANGED, [base64.b64encode(shot).decode() for shot in (shown, hidden)])


def drawn(page, element, canvases, minimum, attempts=10):
    """Poll the canvas coverage, since software WebGL on CI runners can take a while to draw."""
    covered = 0
    for _ in range(attempts):
        page.wait_for_timeout(600)
        covered = changed_pixels(page, element, canvases)
        if covered > minimum:
            break
    return covered


def check_hero_labels(page, animated=False):
    """Every category remains readable and tethered, even without the canvas."""
    svg = page.locator("svg.file-map")
    assert svg.is_visible()
    assert svg.locator(".fm-node").count() == len(HERO_KINDS)
    assert svg.locator(".fm-edge").count() == len(HERO_KINDS)
    for kind, label in HERO_KINDS.items():
        node = svg.locator(f".fm-node.fm-{kind}")
        assert node.locator("text").text_content() == label
        assert label in svg.get_attribute("aria-label")
        assert node.locator("text").is_visible()
        assert node.evaluate("""node => {
            const r = node.querySelector('rect').getBBox(), t = node.querySelector('text').getBBox();
            return t.x >= r.x && t.x + t.width <= r.x + r.width
                && t.y >= r.y && t.y + t.height <= r.y + r.height;
        }"""), (kind, "label spills outside chip")
    assert svg.evaluate("""svg => {
        const nodes = [...svg.querySelectorAll('.fm-node rect')].map(r => r.getBBox());
        return nodes.every((a, i) => nodes.every((b, j) => i === j
            || a.x + a.width <= b.x || b.x + b.width <= a.x
            || a.y + a.height <= b.y || b.y + b.height <= a.y));
    }"""), "overlapping hero labels"
    assert svg.evaluate("""(svg, animated) => {
        const nodes = [...svg.querySelectorAll('.fm-node')];
        const core = svg.querySelector('.fm-core rect').getBBox();
        const contains = (r, p) => p.x >= r.x && p.x <= r.x + r.width
            && p.y >= r.y && p.y <= r.y + r.height;
        const owners = new Set();
        return [...svg.querySelectorAll('.fm-edge')].every(edge => {
            const start = edge.getPointAtLength(0), end = edge.getPointAtLength(edge.getTotalLength());
            const owner = nodes.find(n => contains(n.querySelector('rect').getBBox(), start));
            if (!owner || owners.has(owner)) return false;
            owners.add(owner);
            if (!animated) return contains(core, end);
            const kind = [...owner.classList].find(c => c !== 'fm-node');
            const anchor = svg.querySelector('.fm-anchor.' + kind);
            return edge.classList.contains(kind) && anchor
                && Math.hypot(end.x - Number(anchor.getAttribute('cx')),
                    end.y - Number(anchor.getAttribute('cy'))) < 0.2;
        });
    }""", animated), "a category lost its connection"


def check_landing_buffers(p, url):
    """Inspect actual hero geometry, shader palettes and key/value/recall choreography."""
    module = (ROOT / "site" / "assets" / "landing3d.js").read_text(encoding="utf-8")
    marker = "const view = { fps: 60, optional: false, observers: [], render, dispose() {} };"
    assert module.count(marker) == 1
    module = module.replace(marker, marker + """
    view.inspect = () => ({ data, pointGeo, linkGeo, fxLineGeo, fxPointGeo, U });
    """) + "\nexport { views, KINDS, ANCHORS, palette, PALETTE_SIZE, HI_KIND, CLUSTER_VERT, LINK_VERT };\n"
    browser = p.chromium.launch(args=SWIFTSHADER)
    try:
        context = browser.new_context(reduced_motion="reduce", viewport={"width": 1440, "height": 1050})
        context.route(re.compile(r"/assets/landing3d\.js(?:\?|$)"), lambda route: route.fulfill(
            content_type="text/javascript", body=module))
        page = context.new_page()
        errors = []
        track_errors(page, errors)
        page.goto(f"{url}/index.html?force3d")
        assert hero_state(page)[0] == "ready"
        for theme in ("light", "dark"):
            page.evaluate("theme => document.documentElement.dataset.theme = theme", theme)
            page.wait_for_timeout(150)
            result = page.evaluate("""async () => {
                const m = await import(document.querySelector('script[src*="assets/landing3d.js"]').src);
                const view = m.views.find(v => !v.optional);
                const {data, pointGeo, linkGeo, fxLineGeo, fxPointGeo, U} = view.inspect();
                const P = m.palette();
                const colorMatches = (a, b) => a.every((v, i) => Math.abs(v - b[i]) < 0.00001);
                const counts = data.groups.map(g => g.length);
                const links = m.KINDS.map((_, k) => data.links.filter(([a, b]) =>
                    data.kind[a] === k && data.kind[b] === k).length);
                const indices = [...pointGeo.attributes.aKind.array, ...linkGeo.attributes.aKind.array];
                const choreography = [[6, 6.7], [7, 8.0]].map(([k, t]) => {
                    view.render(t);
                    return data.groups[k].some(i => pointGeo.attributes.aHi.array[i] > 0.1)
                        && fxLineGeo.drawRange.count > 0 && fxPointGeo.drawRange.count > 0
                        && colorMatches([...fxPointGeo.attributes.aCol.array].slice(
                            (fxPointGeo.drawRange.count - 1) * 4,
                            (fxPointGeo.drawRange.count - 1) * 4 + 3), P[m.KINDS[k]]);
                });
                view.render(null);
                const still = [...pointGeo.attributes.aHi.array];
                view.render(null);
                return {
                    kinds: m.KINDS, anchors: Object.keys(m.ANCHORS),
                    counts, links, choreography,
                    stable: still.every((v, i) => v === pointGeo.attributes.aHi.array[i]),
                    valid: indices.every(i => Number.isInteger(i) && i >= 0 && i < m.PALETTE_SIZE),
                    palette: m.KINDS.every((k, i) => colorMatches(
                        [...U.uPal.value].slice(i * 4, i * 4 + 3), P[k])),
                    size: U.uPal.value.length,
                    shaders: [m.CLUSTER_VERT, m.LINK_VERT].every(s => s.includes('uPal[10]'))
                        && m.CLUSTER_VERT.includes(`uPal[${m.HI_KIND}].rgb`),
                    stillKinds: [6, 7].every(k => data.groups[k].some(i => still[i] > 0.1)),
                };
            }""")
            assert result["kinds"] == result["anchors"] == list(HERO_KINDS), result
            assert all(result["counts"]) and len(result["counts"]) == 9, result
            assert result["counts"][6:8] == [21, 7] and result["links"][6:8] == [18, 12], result
            assert result["size"] == 40 and all(result["choreography"]), result
            assert all(result[k] for k in ("stable", "valid", "palette", "shaders", "stillKinds")), result
        assert not errors, errors
        context.close()
    finally:
        browser.close()


def check_landing_previews(p, url, screenshots=None):
    """Reuse the verified workspace captures through existing accessible tabs."""
    browser = p.chromium.launch()
    try:
        for theme in ("light", "dark"):
            context = browser.new_context(color_scheme=theme, reduced_motion="reduce")
            page = context.new_page()
            errors = []
            track_errors(page, errors)
            page.goto(f"{url}/index.html")
            for width in (1440, 390, 320):
                page.set_viewport_size({"width": width, "height": 1050})
                for kind, image in (("overview", f"overview-{'dark' if theme == 'dark' else 'light'}"),
                                    ("kv", "key-value"), ("memory", "memory"), ("graph", "graph")):
                    tab = page.locator(f"#t-{kind}")
                    tab.click()
                    assert tab.get_attribute("aria-selected") == "true"
                    panel = page.locator(f"#p-{kind}")
                    assert panel.is_visible()
                    capture = panel.locator("img")
                    capture.evaluate("img => img.decode()")
                    assert capture.evaluate("img => new URL(img.currentSrc).pathname").endswith(f"current-{image}.png")
                    assert page.locator("#dashboard-preview [role=tabpanel]:visible").count() == 1
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (theme, width)
                    if screenshots and width in (1440, 390):
                        screenshots.mkdir(parents=True, exist_ok=True)
                        page.locator(".landing-shot").screenshot(
                            path=str(screenshots / f"preview-{kind}-{theme}-{width}.png"))
                page.locator("#t-overview").focus()
                page.keyboard.press("ArrowRight")
                assert page.locator("#t-kv").evaluate("e => e === document.activeElement")
                page.keyboard.press("ArrowLeft")
                assert page.locator("#t-overview").get_attribute("aria-selected") == "true"
                for name in ("py", "node", "rust", "docker", "devbox"):
                    page.locator(f"#t-{name}").click()
                    assert page.locator(f"#p-{name}").is_visible()
            assert not errors, errors
            context.close()
        for theme in ("light", "dark"):
            context = browser.new_context(color_scheme=theme, reduced_motion="reduce")
            context.add_init_script("""(() => {
                const get = HTMLCanvasElement.prototype.getContext;
                HTMLCanvasElement.prototype.getContext = function(type, ...args) {
                    return /^webgl/.test(type) ? null : get.call(this, type, ...args);
                };
            })()""")
            page = context.new_page()
            page.goto(f"{url}/index.html")
            assert hero_state(page) == ["off", "no-webgl2"]
            assert page.locator("html").get_attribute("data-theme") == theme
            for width in (1440, 900, 390, 320):
                page.set_viewport_size({"width": width, "height": 1050})
                check_toolkit(page)
                if screenshots and width in (1440, 390):
                    page.locator(".product-map-core").screenshot(
                        path=str(screenshots / f"toolkit-fallback-{theme}-{width}.png"),
                        style="header { visibility: hidden !important; }")
            if screenshots:
                page.set_viewport_size({"width": 390, "height": 1050})
                page.evaluate("scrollTo(0, 0)")
                page.screenshot(path=str(screenshots / f"homepage-{theme}-390.png"), full_page=True)
            for width in (1440, 580):
                page.set_viewport_size({"width": width, "height": 1050})
                check_hero_labels(page)
                assert page.locator("#p-overview img").is_visible()
                if screenshots:
                    page.locator(".hero-split").screenshot(
                        path=str(screenshots / f"hero-fallback-{theme}-{width}.png"))
            page.set_viewport_size({"width": 390, "height": 1050})
            assert not page.locator(".hero-art").is_visible(), "preserve the compact mobile hero"
            assert "key-value" in page.locator(".hero-copy").inner_text()
            assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
            if screenshots:
                page.screenshot(path=str(screenshots / f"index-fallback-{theme}-390.png"))
            context.close()
    finally:
        browser.close()


def check_three_exports(site):
    bundle = (site / "assets" / "vendor" / "three" / "three.min.js").read_text(encoding="utf-8")
    exported = {name.split(" as ")[-1].strip()
                for block in re.findall(r"export\s*\{([^}]*)\}", bundle) for name in block.split(",")}
    used = set()
    for module in ("landing3d.js", "diagrams3d.js"):
        used.update(re.findall(r"\bTHREE\.(\w+)", (site / "assets" / module).read_text(encoding="utf-8")))
    for module in ("graph3d.js", "ambience.js"):
        used.update(re.findall(r"\bTHREE\.(\w+)", (ROOT / "src" / "dashboard" / module).read_text(encoding="utf-8")))
    assert used and used <= exported, sorted(used - exported)


def check_toolkit(page, animated=False):
    assert page.locator("main > section[id]").evaluate_all(
        "sections => sections.map(s => s.id)") == [
            "quickstart", "products", "dashboard-preview", "features", "data-safety"]
    core = page.locator(".product-map-core")
    assert core.locator(".product-map-row").count() == 5
    assert core.locator(".product-map-row").evaluate_all(
        "rows => rows.map(r => [...r.classList].find(c => c !== 'product-map-row'))") == [
            "product-connect", "product-retrieve", "product-store", "product-graph", "product-memory"]
    card = page.locator(".product-memory")
    assert "Key-value & MemoryAI" in card.locator("h3").inner_text()
    assert card.locator('a[href="docs/KEY-VALUE.html"]').inner_text() == "Keys, values, versions"
    assert card.locator('a[href="docs/MEMORY.html"]').inner_text() == "Agent memory (opt-in)"
    assert page.locator('.product-store a[href="docs/MEMORY.html"]').count() == 0
    for kind, label in (("kv", "Key-value lookup"), ("memory", "MemoryAI recall")):
        group = card.locator(f".lane-{kind}")
        assert group.locator("text").text_content() == label
        assert group.locator("text").is_visible()
        assert group.locator(".lane-diagram").is_visible() != animated
        assert group.locator(".lane-diagram path").count() > 0
    assert card.locator(".lane-memory circle").count() == 4
    assert card.locator(".lane-kv rect").count() == 1
    assert card.locator(".lane-fallback").get_attribute("aria-label") == (
        "Key-value lookup connects a key to its value. MemoryAI recall connects a query to remembered episodes.")
    assert card.evaluate("""card => [...card.querySelectorAll('h3, a')].every(e =>
        e.scrollWidth <= e.clientWidth + 1)"""), "fifth card text clips"
    assert card.locator(".lane-fallback text").evaluate_all("""labels => labels.every(label => {
        const m = label.getScreenCTM();
        return parseFloat(getComputedStyle(label).fontSize) * Math.hypot(m.a, m.b) >= 12;
    })"""), "diagram labels must remain readable"
    assert core.locator(".product-map-row").evaluate_all("""rows => rows.every(row =>
        rows.filter(peer => peer.offsetTop === row.offsetTop).every(peer =>
            peer.querySelector('.lane-art').offsetHeight === row.querySelector('.lane-art').offsetHeight))
    """), "illustration dividers must align across each row"
    width = page.viewport_size["width"]
    columns = 5 if width > 1200 else 3 if width > 1080 else 2 if width > 640 else 1
    assert core.evaluate("e => getComputedStyle(e).gridTemplateColumns.split(' ').length") == columns
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
    if animated:
        assert core.locator(".lane-canvas").count() == 1, "all five scenes share one renderer"


def check_intrinsic_memory_diagram(page):
    card = page.locator(".product-memory")
    assert card.evaluate("""card => {
        const svg = card.querySelector('svg'), art = svg.parentElement;
        const s = svg.getBoundingClientRect(), a = art.getBoundingClientRect();
        const h = card.querySelector('h3').getBoundingClientRect();
        return s.width > 0 && s.height === 200 && s.left >= a.left && s.right <= a.right + 1
            && s.top >= a.top && s.bottom <= a.bottom + 1 && s.bottom <= h.top
            && [...svg.querySelectorAll('path')].every(p => getComputedStyle(p).fill === 'none')
            && [...svg.querySelectorAll('.lane-diagram')].every(g =>
                !['none', 'rgb(0, 0, 0)'].includes(getComputedStyle(g).stroke))
            && [...svg.querySelectorAll('text')].every(t => {
                const b = t.getBBox(), m = t.getScreenCTM();
                return b.x >= 0 && b.y >= 0 && b.x + b.width <= 240 && b.y + b.height <= 200
                    && parseFloat(getComputedStyle(t).fontSize) * Math.hypot(m.a, m.b) >= 12;
            });
    }"""), "fallback must be bounded, unfilled, connected and readable without its stylesheet"


def check_cached_upgrade(p, site, screenshots=None):
    """Prime the real HTTP cache at legacy URLs, then navigate to fingerprinted HTML."""
    html = (site / "index.html").read_text(encoding="utf-8")
    legacy_html = re.sub(r"\?v=[0-9a-f]{12}", "", html).encode()
    style = (site / "assets" / "style.css").read_text(encoding="utf-8")
    # Recreate the preceding four-column/150px rules, without fifth-card selectors.
    legacy_style = re.sub(
        r"^.*(?:product-memory|lane-fallback|lane-kv|lane-memory|lane-diagram|max-width: 1200px).*\n",
        "", style, flags=re.MULTILINE).replace(
            "grid-template-columns: repeat(5,", "grid-template-columns: repeat(4,").replace(
            ".lane-art { flex: none; height: 200px;", ".lane-art { flex: none; height: 150px;")
    legacy_style = re.sub(r"\?v=[0-9a-f]{12}", "", legacy_style)
    legacy_assets = {"/assets/style.css": legacy_style.encode()}
    for name in ("site.js", "landing3d.js", "diagrams3d.js"):
        source = (site / "assets" / name).read_text(encoding="utf-8")
        if name == "landing3d.js":
            source = source.replace(', "product-memory": memoryLane', "")
        source = re.sub(r"\?v=[0-9a-f]{12}", "", source)
        legacy_assets[f"/assets/{name}"] = (
            "window.__legacyAssets = (window.__legacyAssets || 0) + 1;\n" + source).encode()

    class UpgradeHandler(QuietHandler):
        def end_headers(self):
            self.send_header("Cache-Control", "public, max-age=600" if self.path.startswith("/assets/")
                             else "no-store")
            super().end_headers()

        def do_GET(self):
            self.server.received.append(self.path)
            resource = urlsplit(self.path)
            if resource.path == "/legacy.html":
                body, mime = legacy_html, "text/html"
            elif not resource.query and resource.path in legacy_assets:
                body = legacy_assets[resource.path]
                mime = "text/css" if resource.path.endswith(".css") else "text/javascript"
            else:
                return super().do_GET()
            self.send_response(200)
            self.send_header("Content-Type", mime)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), partial(UpgradeHandler, directory=str(site)))
    server.received = []
    Thread(target=server.serve_forever, daemon=True).start()
    browser = p.chromium.launch(args=SWIFTSHADER)
    try:
        url = f"http://127.0.0.1:{server.server_port}"
        for theme in ("light", "dark"):
            for width in (1440, 390):
                for animated in (False, True):
                    context = browser.new_context(color_scheme=theme, reduced_motion="reduce",
                                                  viewport={"width": width, "height": 1050})
                    if not animated:
                        context.add_init_script("""(() => {
                            const get = HTMLCanvasElement.prototype.getContext;
                            HTMLCanvasElement.prototype.getContext = function(type, ...args) {
                                return /^webgl/.test(type) ? null : get.call(this, type, ...args);
                            };
                        })()""")
                    page = context.new_page()
                    errors = []
                    track_errors(page, errors)
                    for _ in range(2):
                        before = len(server.received)
                        page.goto(f"{url}/legacy.html" + ("?force3d" if animated else ""))
                        hero_state(page)
                        page.evaluate("document.fonts.ready")
                        assert page.evaluate("window.__legacyAssets") == 3
                        page.locator(".product-map-core").scroll_into_view_if_needed()
                        check_intrinsic_memory_diagram(page)
                        if _ == 0:
                            assert all(path in server.received[before:] for path in legacy_assets)
                            assert page.locator(".product-map-core").evaluate(
                                "e => getComputedStyle(e).gridTemplateColumns.split(' ').length") == (
                                    4 if width == 1440 else 1)
                        else:
                            assert not any(path in legacy_assets for path in server.received[before:]), (
                                "legacy assets were not actually reused from HTTP cache", server.received[before:])
                    before = len(server.received)
                    page.goto(f"{url}/index.html?force3d")
                    assert hero_state(page)[0] == ("ready" if animated else "off")
                    page.locator(".product-map-core").scroll_into_view_if_needed()
                    if animated:
                        page.wait_for_selector(".lanes-3d .lane-canvas", state="attached")
                    page.evaluate("document.fonts.ready")
                    check_toolkit(page, animated=animated)
                    check_intrinsic_memory_diagram(page)
                    assert page.evaluate("typeof window.__legacyAssets") == "undefined"
                    requests = server.received[before:]
                    for asset in (*legacy_assets, "/assets/diagrams3d.css"):
                        assert any(path.startswith(asset + "?v=") for path in requests), (asset, requests)
                    assert not any(path in legacy_assets for path in requests), requests
                    dependencies = ["/assets/theme.css", "/assets/diagrams/palette.css",
                                    "/assets/fonts/outfit-latin-wght-normal.woff2",
                                    "/assets/fonts/jetbrains-mono-latin-wght-normal.woff2"]
                    if animated:
                        dependencies.append("/assets/vendor/three/three.min.js")
                    for asset in dependencies:
                        assert any(path.startswith(asset + "?v=") for path in requests), (asset, requests)
                        assert asset not in requests, (asset, requests)
                    if animated:
                        for kind in ("kv", "memory"):
                            assert drawn(page, page.locator(f".product-memory .lane-{kind}"),
                                         ".lane-canvas", 0.005) > 0.005
                    if screenshots:
                        mode = "webgl" if animated else "fallback"
                        page.locator(".product-map-core").screenshot(
                            path=str(screenshots / f"cache-upgrade-{theme}-{width}-{mode}.png"),
                            style="header, .landing-skip { visibility: hidden !important; }")
                    if not animated:
                        page.evaluate("document.querySelectorAll('link[rel=stylesheet]').forEach(e => e.remove())")
                        check_intrinsic_memory_diagram(page)
                    assert not errors, errors
                    context.close()
    finally:
        browser.close()
        server.shutdown()
        server.server_close()


def check_toolkit_buffers(p, url):
    module = (ROOT / "site" / "assets" / "landing3d.js").read_text(encoding="utf-8")
    marker = "const lanes = [];"
    assert module.count(marker) == 1
    module = module.replace(marker, marker + "\n  view.inspect = () => lanes;\n")
    marker = "return { U, scene, materials, update, theme, still: 1.2, ex: 2.5, ey: 2.5, ez: 0, dir: [0, 0, 1] };"
    assert module.count(marker) == 1
    module = module.replace(marker, marker.replace("return {", "return { layers,"))
    module += "\nexport { views, palette };\n"
    browser = p.chromium.launch(args=SWIFTSHADER)
    try:
        context = browser.new_context(reduced_motion="reduce", viewport={"width": 1440, "height": 1050})
        context.route(re.compile(r"/assets/landing3d\.js(?:\?|$)"), lambda route: route.fulfill(
            content_type="text/javascript", body=module))
        page = context.new_page()
        errors = []
        track_errors(page, errors)
        page.goto(f"{url}/index.html?force3d")
        hero_state(page)
        page.locator(".product-map-core").scroll_into_view_if_needed()
        page.wait_for_selector(".lanes-3d .lane-canvas", state="attached")
        for theme in ("light", "dark"):
            page.evaluate("theme => document.documentElement.dataset.theme = theme", theme)
            page.wait_for_timeout(200)
            result = page.evaluate("""async () => {
                const { views, palette } = await import(document.querySelector('script[src*="assets/landing3d.js"]').src);
                const lanes = views.find(v => v.optional).inspect();
                const lane = lanes.find(l => l.row.classList.contains('product-memory'));
                const P = palette();
                return { count: lanes.length, layers: lane.layers.map(layer => {
                    const {kind, uniforms, nodes, edges, points, lines, pulse} = layer;
                    lane.update(0.3);
                    const before = [...pulse.geo.attributes.position.array];
                    lane.update(1.7);
                    const moving = before.some((v, i) => v !== pulse.geo.attributes.position.array[i]);
                    lane.update(lane.still);
                    const still = [...pulse.geo.attributes.position.array];
                    lane.update(lane.still);
                    return { kind, nodes: points.geo.attributes.position.count,
                        lines: lines.geo.attributes.position.count,
                        connected: edges.every(([a, b]) => a !== b && nodes[a] && nodes[b]),
                        moving, still: still.every((v, i) => v === pulse.geo.attributes.position.array[i]),
                        colored: uniforms.uCol.value.every((v, i) => Math.abs(v - P[kind][i]) < 0.00001),
                        shared: uniforms.uPx === lane.U.uPx && uniforms.uScale === lane.U.uScale,
                        highlight: [...points.geo.attributes.aB.array].some(v => v > 0)
                    };
                }) };
            }""")
            assert result["count"] == 5, result
            assert [v["kind"] for v in result["layers"]] == ["kv", "memory"], result
            assert [(v["nodes"], v["lines"]) for v in result["layers"]] == [(6, 12), (4, 12)], result
            assert all(v[k] for v in result["layers"] for k in (
                "connected", "moving", "still", "colored", "shared", "highlight")), result
        page.emulate_media(media="print")
        assert not page.locator(".lane-canvas").is_visible()
        assert page.locator(".product-memory .lane-kv .lane-diagram").is_visible()
        assert page.locator(".product-memory .lane-memory .lane-diagram").is_visible()
        assert not errors, errors
        context.close()
    finally:
        browser.close()


def check_forced_3d(p, url, screenshots=None):
    """Force WebGL2 through SwiftShader and check the hero and toolkit scenes draw without errors."""
    browser = p.chromium.launch(args=SWIFTSHADER)
    try:
        for theme in ("light", "dark"):
            for motion in ("no-preference", "reduce"):
                context = browser.new_context(color_scheme=theme, reduced_motion=motion,
                                              viewport={"width": 1440, "height": 1050})
                errors = []
                page = context.new_page()
                track_errors(page, errors)
                page.goto(f"{url}/index.html?force3d")
                state = hero_state(page)
                if state == ["off", "no-webgl2"]:
                    context.close()
                    return False
                assert state[0] == "ready", (theme, motion, state, errors)
                assert page.evaluate("document.querySelector('.hero-split').classList.contains('has-3d')")
                assert page.evaluate("(c => c.width > 0 && c.height > 0)(document.querySelector('.hero-canvas'))")
                assert page.locator("svg.file-map .fm-anchor").count() == len(HERO_KINDS)
                check_hero_labels(page, animated=True)
                if motion == "reduce":
                    hero = drawn(page, page.locator(".hero-art"), ".hero-canvas", 0.02)
                    assert hero > 0.02, (theme, "hero canvas drew too little", hero)
                else:
                    page.wait_for_selector("svg.file-map .fm-status.fm-commit", state="attached", timeout=20000)
                    assert page.evaluate(EDGES) == [8, 8, 8]
                    if screenshots:
                        screenshots.mkdir(parents=True, exist_ok=True)
                        page.screenshot(path=str(screenshots / f"index-3d-{theme}-1440.png"))
                        page.locator(".hero-split").screenshot(
                            path=str(screenshots / f"hero-3d-{theme}-1440.png"))
                core = page.locator(".product-map-core")
                core.scroll_into_view_if_needed()
                page.wait_for_selector(".product-map-core.lanes-3d .lane-canvas", state="attached", timeout=20000)
                if motion == "reduce":
                    lanes = drawn(page, core, ".lane-canvas", 0.01)
                    assert lanes > 0.01, (theme, "toolkit canvas drew too little", lanes)
                    check_toolkit(page, animated=True)
                    for kind in ("kv", "memory"):
                        coverage = drawn(page, page.locator(f".product-memory .lane-{kind}"), ".lane-canvas", 0.005)
                        assert coverage > 0.005, (theme, kind, "no actual canvas pixels", coverage)
                    if screenshots:
                        core.screenshot(path=str(screenshots / f"toolkit-{theme}-1440.png"))
                        page.evaluate("scrollTo(0, 0)")
                        page.screenshot(path=str(screenshots / f"homepage-{theme}-1440.png"), full_page=True)
                else:
                    page.wait_for_timeout(1500)
                page.locator("#theme-toggle").click()
                page.wait_for_timeout(500)
                assert page.evaluate(HERO_STATE)[0] == "ready"
                if motion == "no-preference":
                    # The canvases follow layout changes without widening the page.
                    for width in (1150, 900, 640, 390, 320):
                        page.set_viewport_size({"width": width, "height": 1050})
                        core.scroll_into_view_if_needed()
                        page.wait_for_timeout(400)
                        check_toolkit(page, animated=True)
                        if screenshots and width in (900, 390):
                            current_theme = page.locator("html").get_attribute("data-theme")
                            core.screenshot(path=str(screenshots / f"toolkit-{current_theme}-{width}.png"),
                                            style="header { visibility: hidden !important; }")
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (theme, width)
                        assert page.evaluate(HERO_STATE)[0] == "ready", (theme, width)
                        if width > 560:
                            check_hero_labels(page, animated=True)
                assert not errors, errors
                context.close()
        # Without ?force3d, software WebGL must keep the SVG illustration and skip the 3D bundle.
        page = browser.new_page(viewport={"width": 1440, "height": 1050})
        errors = []
        track_errors(page, errors)
        page.goto(f"{url}/index.html")
        state = hero_state(page)
        if "swiftshader" in page.evaluate(RENDERER).lower():
            assert state == ["off", "software-gl"], state
            assert page.locator(".hero-canvas").count() == 0
            assert page.locator("svg.file-map").is_visible()
            check_hero_labels(page)
        assert not errors, errors
        page.close()
    finally:
        browser.close()
    return True


def decode_image(page, image, theme):
    image.scroll_into_view_if_needed()
    page.wait_for_function(
        """([img, suffix]) => img.complete && img.naturalWidth > 0
            && new URL(img.currentSrc).pathname.endsWith(suffix)""",
        arg=[image.element_handle(), f"-{theme}.svg"],
    )
    image.evaluate("img => img.decode()")


def check_diagram_palette(p, url):
    """Inspect actual GPU color attributes through a test-only module export."""
    browser = p.chromium.launch(args=SWIFTSHADER)
    try:
        context = browser.new_context(reduced_motion="reduce")
        module = (ROOT / "site" / "assets" / "diagrams3d.js").read_text(encoding="utf-8")
        context.route(re.compile(r"/assets/diagrams3d\.js(?:\?|$)"), lambda route: route.fulfill(
            content_type="text/javascript", body=module + "\nexport { views };\n"))
        page = context.new_page()
        errors, warnings = [], []
        track_errors(page, errors)
        page.on("console", lambda message: message.type == "warning" and warnings.append(message.text))
        inspect = """async () => {
            const {views} = await import(document.querySelector('script[src*="assets/diagrams3d.js"]').src);
            return views.map(v => ({
                name: v.picture.closest('figure')?.dataset.diagram || v.image.dataset.light,
                roles: v.edges.map(e => e.role),
                colors: [...v.pulses.geometry.getAttribute('color').array],
                halos: v.halos.map(h => [h.role, ...h.mesh.material.uniforms.uColor.value]),
            }));
        }"""
        for path, count in (("architecture.html", 14), ("index.html", 2)):
            page.goto(f"{url}/{path}?force3d")
            page.wait_for_function("document.documentElement.dataset.diagrams3d === 'ready'")
            views = page.evaluate(inspect)
            assert len(views) == count
            for view in views:
                assert all(0 <= role < 5 for role in view["roles"]), view
                assert len(view["colors"]) == 3 * len(view["roles"])
            if count == 14:
                layers = next(v for v in views if v["name"] == "layers")
                assert layers["roles"] == [2, 1, 2, 2], layers
                bindings = next(v for v in views if v["name"] == "bindings")
                # The published sequence SVG styles every actor as interface purple.
                assert bindings["roles"] == [0, 0, 0, 0, 0, 0], bindings
            # Distinct known channels make stale halo uniforms / pulse buffers unambiguous,
            # including rgb() tokens rather than only hexadecimal palette values.
            page.add_style_tag(content="""html[data-theme="light"] {
                --diagram-interface: rgb(255, 0, 0); --diagram-search: rgb(0, 255, 0);
                --diagram-storage: rgb(0, 0, 255); --diagram-operations: rgb(255, 255, 0);
                --diagram-error: rgb(255, 0, 255); }
                html[data-theme="dark"] {
                --diagram-interface: rgb(0, 255, 255); --diagram-search: rgb(255, 0, 255);
                --diagram-storage: rgb(255, 255, 0); --diagram-operations: rgb(0, 0, 255);
                --diagram-error: rgb(0, 255, 0); }""")
            palettes = {
                "light": [[1, 0, 0], [0, 1, 0], [0, 0, 1], [1, 1, 0], [1, 0, 1]],
                "dark": [[0, 1, 1], [1, 0, 1], [1, 1, 0], [0, 0, 1], [0, 1, 0]],
            }
            for theme, palette in palettes.items():
                page.evaluate("theme => document.documentElement.dataset.theme = theme", theme)
                page.wait_for_timeout(100)
                for view in page.evaluate(inspect):
                    assert view["colors"] == [c for role in view["roles"] for c in palette[role]], view
                    assert all(values[1:] == palette[values[0]] for values in view["halos"]), view
        assert not errors, errors
        assert not warnings, warnings
        # A malformed response is isolated to one figure with an actionable diagnostic.
        context.route("**/diagrams/layers-light.svg", lambda route: route.fulfill(
            content_type="image/svg+xml", body="<svg><broken>") if
            route.request.resource_type == "fetch" else route.continue_())
        page.goto(f"{url}/architecture.html?force3d")
        page.wait_for_function("document.documentElement.dataset.diagrams3d === 'ready'")
        picture = page.locator('figure[data-diagram="layers"] picture')
        assert picture.get_attribute("data-diagrams3d-reason") == "unsupported-svg"
        assert picture.locator("img").is_visible()
        assert page.locator(".diagram-3d-stage").count() == 13
        assert any("VLite diagram kept its SVG:" in warning and "layers-light.svg" in warning for warning in warnings)
        assert not errors, errors
        context.close()
    finally:
        browser.close()


def check_diagram_scenes(p, url, screenshots=None):
    """Actual scissored WebGL output, not CSS panels masquerading as 3D."""
    browser = p.chromium.launch(args=SWIFTSHADER)
    try:
        for theme in ("light", "dark"):
            for motion in ("reduce", "no-preference"):
                context = browser.new_context(color_scheme=theme, reduced_motion=motion,
                                              viewport={"width": 1440, "height": 1050})
                context.add_init_script("""(() => {
                    const get = HTMLCanvasElement.prototype.getContext;
                    window.webglCanvases = new Set();
                    HTMLCanvasElement.prototype.getContext = function(type, ...args) {
                        const result = get.call(this, type, ...args);
                        if (result && /^webgl/.test(type)) window.webglCanvases.add(this);
                        return result;
                    };
                })()""")
                external, errors = [], []
                context.route("**/*", lambda route: route.continue_() if route.request.url.startswith(url) else
                              (external.append(route.request.url), route.abort())[-1])
                page = context.new_page()
                track_errors(page, errors)
                for path, count in (("index.html", 2), ("architecture.html", 14)):
                    page.goto(f"{url}/{path}?force3d")
                    page.wait_for_function("document.documentElement.dataset.diagrams3d !== 'loading'")
                    assert page.locator("html").get_attribute("data-diagrams3d") == "ready", (
                        path, page.locator("html").get_attribute("data-diagrams3d-reason"), errors)
                    stages = page.locator(".diagram-3d-stage")
                    assert stages.count() == count
                    assert page.locator(".diagrams-3d-canvas").count() == 1
                    assert page.locator(".diagrams-3d-canvas").get_attribute("aria-hidden") == "true"
                    # The landing hero/toolkit already own their existing contexts.
                    assert page.evaluate("""[...webglCanvases].filter(c =>
                        !c.matches('.hero-canvas, .lane-canvas')).length""") == 1
                    widths = (1440, 900, 390, 320) if motion == "reduce" else (1440,)
                    for width in widths:
                        page.set_viewport_size({"width": width, "height": 1050})
                        for index, stage in enumerate(stages.all()):
                            image = stage.locator("img")
                            decode_image(page, image, theme)
                            page.wait_for_timeout(100)
                            assert int(stage.get_attribute("data-edge-count")) > 0
                            assert int(stage.get_attribute("data-halo-count")) > 0
                            assert image.is_visible() and image.get_attribute("alt")
                            if motion == "reduce":
                                page.wait_for_timeout(100)
                                before = stage.get_attribute("data-render-count")
                                page.wait_for_timeout(160)
                                assert stage.get_attribute("data-render-count") == before
                                covered = changed_pixels(page, stage, ".diagrams-3d-canvas")
                                assert covered > 0.0005, (path, theme, width, index, "no WebGL pixels", covered)
                            elif width == 1440:
                                assert drawn(page, stage, ".diagrams-3d-canvas", 0.0005, attempts=3) > 0.0005
                            assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (path, width)
                        if path == "architecture.html" and width == 1440 and motion == "reduce" and screenshots:
                            screenshots.mkdir(parents=True, exist_ok=True)
                            stages.first.screenshot(path=str(screenshots / f"architecture-3d-{theme}.png"))
                        if path == "index.html" and width == 1440 and motion == "reduce" and screenshots:
                            screenshots.mkdir(parents=True, exist_ok=True)
                            page.locator(".deployment-options").screenshot(
                                path=str(screenshots / f"deployment-3d-{theme}.png"))
                    if motion == "no-preference":
                        for name in (("commit", "bindings") if count == 14 else ()):
                            stage = page.locator(f'figure[data-diagram="{name}"] picture')
                            stage.scroll_into_view_if_needed()
                            page.wait_for_timeout(150)
                            step = int(stage.get_attribute("data-sequence-step"))
                            page.wait_for_function(
                                "([stage, step]) => Number(stage.dataset.sequenceStep) !== step",
                                arg=[stage.element_handle(), step], timeout=3000)
                            after = int(stage.get_attribute("data-sequence-step"))
                            assert after == (step + 1) % int(stage.get_attribute("data-edge-count")), (name, step, after)
                        last = stages.last
                        last.scroll_into_view_if_needed()
                        page.wait_for_timeout(200)
                        frozen = stages.first.get_attribute("data-render-count")
                        page.wait_for_timeout(200)
                        if count == 14:
                            assert stages.first.get_attribute("data-render-count") == frozen
                    page.locator("#theme-toggle").click()
                    page.wait_for_timeout(150)
                    assert page.locator("html").get_attribute("data-diagrams3d") == "ready"
                    # Reset persisted manual preference before testing the next document.
                    page.evaluate("localStorage.removeItem('vlite-theme')")
                assert not errors, errors
                assert not external, external
                context.close()
        for fallback in ("no-webgl2", "save-data", "no-js"):
            context = browser.new_context(java_script_enabled=fallback != "no-js", color_scheme="dark")
            if fallback == "no-webgl2":
                context.add_init_script("""(() => {
                    const get = HTMLCanvasElement.prototype.getContext;
                    HTMLCanvasElement.prototype.getContext = function(type, ...args) {
                        return /^webgl/.test(type) ? null : get.call(this, type, ...args);
                    };
                })()""")
            elif fallback == "save-data":
                context.add_init_script("Object.defineProperty(navigator, 'connection', {value: {saveData: true}})")
            page = context.new_page()
            errors = []
            track_errors(page, errors)
            for path, count in (("index.html", 2), ("architecture.html", 14)):
                page.goto(f"{url}/{path}?force3d")
                page.wait_for_load_state("networkidle")
                assert page.locator(".diagrams-3d-canvas, .diagram-3d-stage").count() == 0
                images = page.locator(".deployment-options img" if count == 2 else ".diagram img")
                assert images.count() == count
                for image in images.all():
                    decode_image(page, image, "dark")
                if fallback != "no-js":
                    assert page.locator("html").get_attribute("data-diagrams3d-reason") == fallback
            assert not errors, errors
            context.close()
    finally:
        browser.close()


def check(site, screenshots=None):
    assets = site / "assets" / "diagrams"
    palette = json.loads((assets / "palette.json").read_text(encoding="utf-8"))
    css = (assets / "palette.css").read_text(encoding="utf-8")
    for role, values in palette.items():
        assert f"--diagram-{role}: {values['color']}" in css
    html = (site / "architecture.html").read_text(encoding="utf-8")
    assert html.count('data-diagram="') == 14
    assert 'href="#file-contract">Storage map</a>' in html
    assert 'assets/diagrams/storage-map-light.svg' in html
    assert 'assets/diagrams/storage-map-dark.svg' in html
    assert "cdn.jsdelivr.net" not in html
    assert 'site/assets/diagrams/layers-light.svg' in (ROOT / "README.md").read_text(encoding="utf-8")
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    for theme in ("light", "dark"):
        render = f"architecture-3d-{theme}.png"
        assert f"site/assets/{render}" in readme
        assert (site / "assets" / render).read_bytes().startswith(b"\x89PNG\r\n\x1a\n")
    for image in assets.glob("*.svg"):
        text = image.read_text(encoding="utf-8")
        svg = ET.fromstring(text)
        assert svg.attrib.get("viewBox"), image
        assert not re.search(r"<(?:script|foreignObject)\b", text), image
        assert not re.search(r'(?:href|src)="https?://', text), image
    assert len(list(assets.glob("*.svg"))) == 34
    for mode in ("embedded", "self-hosted"):
        for theme in ("light", "dark"):
            assert (assets / f"deployment-{mode}-{theme}.svg").is_file()
    check_three_exports(site)
    server = ThreadingHTTPServer(("127.0.0.1", 0), partial(QuietHandler, directory=str(site)))
    Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_port}"
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch()
            for theme in ("light", "dark"):
                context = browser.new_context(color_scheme=theme)
                external = []
                context.route("**/*", lambda route: route.continue_() if route.request.url.startswith(url) else
                              (external.append(route.request.url), route.abort())[-1])
                page = context.new_page()
                errors = []
                track_errors(page, errors)
                for width in (1440, 900, 390, 320):
                    page.set_viewport_size({"width": width, "height": 1050})
                    for path in ("architecture.html", "index.html", "features.html"):
                        page.goto(f"{url}/{path}")
                        page.wait_for_load_state("networkidle")
                        if path == "index.html":
                            # Without a usable GPU the 3D view must step aside and keep the SVG map.
                            state = hero_state(page)
                            assert state[0] == "ready" or state[1] in ("no-webgl2", "software-gl", "save-data"), (
                                width, state, errors)
                            if width > 560:
                                assert page.locator("svg.file-map").is_visible()
                                check_hero_labels(page, animated=state[0] == "ready")
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (
                            path, width, page.evaluate("""[...document.querySelectorAll('body *')]
                              .filter(e => e.getBoundingClientRect().right > innerWidth + 1)
                              .slice(0, 8).map(e => [e.tagName, e.className])"""))
                        assert page.locator(".spatial-diagram").count() == 0
                        if path == "architecture.html":
                            images = page.locator(".diagram img")
                            assert images.count() == 15
                            for image in images.all():
                                decode_image(page, image, theme)
                                assert image.evaluate("img => img.naturalWidth > 0 && img.alt.length > 30")
                                assert image.evaluate("img => new URL(img.currentSrc).pathname").endswith(f"-{theme}.svg")
                            storage_map = page.locator(".storage-map")
                            assert storage_map.locator("img").is_visible()
                            assert storage_map.locator("picture").evaluate("e => e.scrollWidth >= 900")
                            if width < 900:
                                assert storage_map.evaluate("e => e.scrollWidth > e.clientWidth")
                            page.locator("#layers").scroll_into_view_if_needed()
                        elif path == "index.html":
                            images = page.locator(".deployment-options img")
                            assert images.count() == 2
                            for image in images.all():
                                decode_image(page, image, theme)
                                assert image.evaluate("img => new URL(img.currentSrc).pathname").endswith(f"-{theme}.svg")
                                assert image.evaluate("img => img.naturalWidth > 0 && img.alt.length > 30")
                        else:
                            assert page.locator(".feature-catalog section[id]").count() == 11
                            assert page.locator("table caption").count() == 8
                            assert page.locator('nav[aria-label="Primary"] a[aria-current="page"]').inner_text() == "Features"
                            page.locator('.docs-sidebar a[href="#security"]').click()
                            page.wait_for_function("location.hash === '#security'")
                            assert page.locator("#security h2").inner_text() == "Security and access control"
                            page.locator('.docs-sidebar a[href="#graph"]').click()
                            page.wait_for_function("location.hash === '#graph'")
                            assert page.locator("#graph h2").inner_text() == "Property graphs"
                            assert page.locator("#graph").evaluate(
                                "e => e.compareDocumentPosition(document.getElementById('requirements')) & Node.DOCUMENT_POSITION_FOLLOWING")
                        if screenshots and width in (1440, 390):
                            screenshots.mkdir(parents=True, exist_ok=True)
                            page.screenshot(path=str(screenshots / f"{Path(path).stem}-{theme}-{width}.png"))
                            if path == "architecture.html":
                                page.locator(".diagram").first.screenshot(path=str(screenshots / f"layers-{theme}-{width}.png"))
                page.goto(f"{url}/architecture.html")
                page.locator("#theme-toggle").click()
                first = page.locator(".diagram img").first
                opposite = "dark" if theme == "light" else "light"
                decode_image(page, first, opposite)
                assert first.evaluate("img => new URL(img.currentSrc).pathname").endswith(f"-{opposite}.svg")
                page.emulate_media(color_scheme=opposite)
                page.emulate_media(color_scheme=theme)
                assert page.locator("html").get_attribute("data-theme") == opposite, "A manual theme must survive system changes"
                assert not external, external
                assert not errors, errors
                context.close()
            context = browser.new_context(java_script_enabled=False, color_scheme="dark")
            page = context.new_page()
            page.goto(f"{url}/architecture.html")
            image = page.locator(".diagram img").first
            decode_image(page, image, "dark")
            assert image.evaluate("img => new URL(img.currentSrc).pathname").endswith("-dark.svg")
            page.goto(f"{url}/index.html")
            assert page.locator("svg.file-map").is_visible()
            assert page.locator(".hero-split").get_attribute("data-3d") is None
            for width in (1440, 580):
                page.set_viewport_size({"width": width, "height": 1050})
                check_hero_labels(page)
                check_toolkit(page)
                if screenshots:
                    page.locator(".hero-split").screenshot(
                        path=str(screenshots / f"hero-no-js-{width}.png"))
            context.close()
            browser.close()
            forced = check_forced_3d(p, url, screenshots)
            if forced:
                check_landing_buffers(p, url)
                check_toolkit_buffers(p, url)
            check_landing_previews(p, url, screenshots)
            check_cached_upgrade(p, site, screenshots)
            check_diagram_scenes(p, url, screenshots)
            check_diagram_palette(p, url)
    finally:
        server.shutdown()
        server.server_close()
    print("Checked 32 standalone SVGs, shared palette, themes, no-JS, feature navigation and responsive pages.")
    print("Checked forced WebGL2 hero and toolkit scenes." if forced else
          "Skipped forced 3D checks: this Chromium has no WebGL2, even through SwiftShader.")
    print("Checked eight hero labels/connections, key-value and memory geometry/choreography, "
          "shader palettes, reduced motion and four keyboard-accessible dashboard previews.")
    print("Checked Toolkit before Dashboard, five responsive cards, separate lookup/recall geometry, "
          "shared renderer, actual canvas pixels, themes and SVG fallbacks.")
    print("Checked real cached-unversioned-to-fingerprinted upgrades and intrinsically bounded/unfilled "
          "SVG fallback with stale or missing CSS, light/dark, desktop/mobile and WebGL/fallback.")
    print("Checked shared WebGL pixels on both deployment diagrams and all 14 architecture figures, "
          "light/dark, four widths, motion, ordered sequences, offscreen pause, context count and fallbacks.")


if __name__ == "__main__":
    check(Path(sys.argv[1]), Path(sys.argv[2]) if len(sys.argv) > 2 else None)
