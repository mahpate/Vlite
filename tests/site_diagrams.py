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


def decode_image(page, image, theme):
    image.scroll_into_view_if_needed()
    page.wait_for_function(
        """([img, suffix]) => img.complete && img.naturalWidth > 0
            && img.currentSrc.endsWith(suffix)""",
        arg=[image.element_handle(), f"-{theme}.svg"],
    )
    image.evaluate("img => img.decode()")


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
    assert len(list(assets.glob("*.svg"))) == 32
    for mode in ("embedded", "self-hosted"):
        for theme in ("light", "dark"):
            assert (assets / f"deployment-{mode}-{theme}.svg").is_file()
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
                    for path in ("architecture.html", "index.html", "features.html"):
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
                assert not external, external
                context.close()
            context = browser.new_context(java_script_enabled=False, color_scheme="dark")
            page = context.new_page()
            page.goto(f"{url}/architecture.html")
            image = page.locator(".diagram img").first
            decode_image(page, image, "dark")
            assert image.evaluate("img => img.currentSrc").endswith("-dark.svg")
            context.close()
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
    print("Checked 32 standalone SVGs, shared palette, themes, no-JS, feature navigation and responsive pages.")


if __name__ == "__main__":
    check(Path(sys.argv[1]), Path(sys.argv[2]) if len(sys.argv) > 2 else None)
