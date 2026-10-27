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
import xml.etree.ElementTree as ET

from playwright.sync_api import sync_playwright


ROOT = Path(__file__).resolve().parents[1]
SWIFTSHADER = ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"]
HERO_SETTLED = "['ready', 'off'].includes(document.querySelector('.hero-split').getAttribute('data-3d'))"
HERO_STATE = "(h => [h.getAttribute('data-3d'), h.getAttribute('data-3d-reason')])(document.querySelector('.hero-split'))"
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
                assert page.locator("svg.file-map .fm-anchor").count() == 6
                if motion == "reduce":
                    hero = drawn(page, page.locator(".hero-art"), ".hero-canvas", 0.02)
                    assert hero > 0.02, (theme, "hero canvas drew too little", hero)
                else:
                    page.wait_for_selector("svg.file-map .fm-status.fm-commit", state="attached", timeout=20000)
                    assert page.evaluate(EDGES) == [6, 6, 6]
                    if screenshots:
                        screenshots.mkdir(parents=True, exist_ok=True)
                        page.screenshot(path=str(screenshots / f"index-3d-{theme}-1440.png"))
                core = page.locator(".product-map-core")
                core.scroll_into_view_if_needed()
                page.wait_for_selector(".product-map-core.lanes-3d .lane-canvas", state="attached", timeout=20000)
                if motion == "reduce":
                    lanes = drawn(page, core, ".lane-canvas", 0.01)
                    assert lanes > 0.01, (theme, "toolkit canvas drew too little", lanes)
                else:
                    page.wait_for_timeout(1500)
                page.locator("#theme-toggle").click()
                page.wait_for_timeout(500)
                assert page.evaluate(HERO_STATE)[0] == "ready"
                if motion == "no-preference":
                    # The canvases follow layout changes without widening the page.
                    for width in (900, 390, 320):
                        page.set_viewport_size({"width": width, "height": 1050})
                        page.wait_for_timeout(400)
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (theme, width)
                        assert page.evaluate(HERO_STATE)[0] == "ready", (theme, width)
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
        assert not errors, errors
        page.close()
    finally:
        browser.close()
    return True


def decode_image(page, image, theme):
    image.scroll_into_view_if_needed()
    page.wait_for_function(
        """([img, suffix]) => img.complete && img.naturalWidth > 0
            && img.currentSrc.endsWith(suffix)""",
        arg=[image.element_handle(), f"-{theme}.svg"],
    )
    image.evaluate("img => img.decode()")


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
    assert len(list(assets.glob("*.svg"))) == 32
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
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (
                            path, width, page.evaluate("""[...document.querySelectorAll('body *')]
                              .filter(e => e.getBoundingClientRect().right > innerWidth + 1)
                              .slice(0, 8).map(e => [e.tagName, e.className])"""))
                        assert page.locator(".spatial-diagram").count() == 0
                        if path == "architecture.html":
                            images = page.locator(".diagram img")
                            assert images.count() == 14
                            for image in images.all():
                                decode_image(page, image, theme)
                                assert image.evaluate("img => img.naturalWidth > 0 && img.alt.length > 30")
                                assert image.evaluate("img => img.currentSrc").endswith(f"-{theme}.svg")
                            page.locator("#layers").scroll_into_view_if_needed()
                        elif path == "index.html":
                            images = page.locator(".deployment-options img")
                            assert images.count() == 2
                            for image in images.all():
                                decode_image(page, image, theme)
                                assert image.evaluate("img => img.currentSrc").endswith(f"-{theme}.svg")
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
                assert first.evaluate("img => img.currentSrc").endswith(f"-{opposite}.svg")
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
            assert image.evaluate("img => img.currentSrc").endswith("-dark.svg")
            page.goto(f"{url}/index.html")
            assert page.locator("svg.file-map").is_visible()
            assert page.locator(".hero-split").get_attribute("data-3d") is None
            context.close()
            browser.close()
            forced = check_forced_3d(p, url, screenshots)
            check_diagram_scenes(p, url, screenshots)
    finally:
        server.shutdown()
        server.server_close()
    print("Checked 32 standalone SVGs, shared palette, themes, no-JS, feature navigation and responsive pages.")
    print("Checked forced WebGL2 hero and toolkit scenes." if forced else
          "Skipped forced 3D checks: this Chromium has no WebGL2, even through SwiftShader.")
    print("Checked shared WebGL pixels on both deployment diagrams and all 14 architecture figures, "
          "light/dark, four widths, motion, ordered sequences, offscreen pause, context count and fallbacks.")


if __name__ == "__main__":
    check(Path(sys.argv[1]), Path(sys.argv[2]) if len(sys.argv) > 2 else None)
