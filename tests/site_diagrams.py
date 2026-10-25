"""Check shared diagram assets, theme switching and narrow layouts.

Usage: python tests/site_diagrams.py <assembled-site> [screenshot-directory]
Requires tests/requirements-dashboard.txt.
"""

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


class QuietHandler(SimpleHTTPRequestHandler):
    def log_message(self, *_):
        pass


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
    for image in assets.glob("*.svg"):
        text = image.read_text(encoding="utf-8")
        svg = ET.fromstring(text)
        assert svg.attrib.get("viewBox"), image
        assert not re.search(r"<(?:script|foreignObject)\b", text), image
        assert not re.search(r'(?:href|src)="https?://', text), image
    assert len(list(assets.glob("*.svg"))) == 28
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
                for width in (1440, 900, 390, 320):
                    page.set_viewport_size({"width": width, "height": 1050})
                    for path in ("architecture.html", "index.html"):
                        page.goto(f"{url}/{path}")
                        page.wait_for_load_state("networkidle")
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (
                            path, width, page.evaluate("""[...document.querySelectorAll('body *')]
                              .filter(e => e.getBoundingClientRect().right > innerWidth + 1)
                              .slice(0, 8).map(e => [e.tagName, e.className])"""))
                        if path == "architecture.html":
                            images = page.locator(".diagram img")
                            assert images.count() == 14
                            for image in images.all():
                                image.scroll_into_view_if_needed()
                                image.evaluate("img => img.decode()")
                                assert image.evaluate("img => img.naturalWidth > 0 && img.alt.length > 30")
                                assert image.evaluate("img => img.currentSrc").endswith(f"-{theme}.svg")
                            page.locator("#layers").scroll_into_view_if_needed()
                        if screenshots and width in (1440, 390):
                            screenshots.mkdir(parents=True, exist_ok=True)
                            page.screenshot(path=str(screenshots / f"{Path(path).stem}-{theme}-{width}.png"))
                            if path == "architecture.html":
                                page.locator(".diagram").first.screenshot(path=str(screenshots / f"layers-{theme}-{width}.png"))
                page.goto(f"{url}/architecture.html")
                page.locator("#theme-toggle").click()
                first = page.locator(".diagram img").first
                first.scroll_into_view_if_needed()
                first.evaluate("img => img.decode()")
                opposite = "dark" if theme == "light" else "light"
                assert first.evaluate("img => img.currentSrc").endswith(f"-{opposite}.svg")
                assert not external, external
                context.close()
            context = browser.new_context(java_script_enabled=False, color_scheme="dark")
            page = context.new_page()
            page.goto(f"{url}/architecture.html")
            image = page.locator(".diagram img").first
            image.scroll_into_view_if_needed()
            assert image.evaluate("img => img.currentSrc").endswith("-dark.svg")
            image.evaluate("img => img.decode()")
            context.close()
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
    print("Checked 28 standalone SVGs, shared palette, both themes, theme toggle, no-JS and responsive pages.")


if __name__ == "__main__":
    check(Path(sys.argv[1]), Path(sys.argv[2]) if len(sys.argv) > 2 else None)
