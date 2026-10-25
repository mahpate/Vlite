"""Website tour checks. Requires tests/requirements-dashboard.txt.

Usage: python tests/dashboard_tours.py <assembled-site> [screenshot-directory]
"""

from functools import partial
from html.parser import HTMLParser
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import struct
import sys
from threading import Thread

from playwright.sync_api import expect, sync_playwright


class Tours(HTMLParser):
    def __init__(self, text):
        super().__init__()
        self.buttons = []
        self.feed(text)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "button" and "data-tour" in attrs:
            self.buttons.append(attrs)


class QuietHandler(SimpleHTTPRequestHandler):
    def log_message(self, *_):
        pass


def check(root, screenshots=None):
    buttons = Tours((root / "dashboard.html").read_text(encoding="utf-8")).buttons
    assert len(buttons) == 3
    for button in buttons:
        data = (root / button["data-tour"]).read_bytes()
        assert data[:8] == b"\x89PNG\r\n\x1a\n"
        assert len(data) < 3_000_000
        position, frames, duration, expected = 8, 0, 0, 0
        while position < len(data):
            size = struct.unpack_from(">I", data, position)[0]
            kind = data[position + 4:position + 8]
            chunk = data[position + 8:position + 8 + size]
            if kind == b"IHDR":
                assert struct.unpack_from(">II", chunk) == (1440, 1100)
            elif kind == b"acTL":
                expected, loops = struct.unpack(">II", chunk)
                assert loops == 1
            elif kind == b"fcTL":
                numerator, denominator = struct.unpack_from(">HH", chunk, 20)
                duration += 1000 * numerator / (denominator or 100)
                frames += 1
            position += size + 12
        assert frames == expected and frames > 1
        assert duration == int(button["data-duration"])
        poster = (root / button["data-poster"]).read_bytes()
        assert b"acTL" not in poster

    server = ThreadingHTTPServer(("127.0.0.1", 0), partial(QuietHandler, directory=str(root)))
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch()
            page = browser.new_page(reduced_motion="no-preference")
            errors, animations = [], []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.on("request", lambda request: animations.append(request.url)
                    if any(request.url.endswith(b["data-tour"]) for b in buttons) else None)
            base = f"http://127.0.0.1:{server.server_port}"
            for width in (1440, 900, 390, 320):
                page.set_viewport_size({"width": width, "height": 1000})
                for path in ("/", "/dashboard.html"):
                    page.goto(base + path)
                    for theme in ("light", "dark"):
                        page.evaluate("(theme) => document.documentElement.dataset.theme = theme", theme)
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (width, path)
                        if path == "/":
                            nav = page.get_by_role("navigation", name="Primary")
                            assert nav.get_by_role("link").count() == 11
                            for label in ("Products", "API", "Architecture", "About", "ASENDME"):
                                expect(nav.get_by_role("link", name=label, exact=True)).to_have_count(1)
                        elif screenshots and width in (1440, 390):
                            screenshots.mkdir(parents=True, exist_ok=True)
                            page.locator(".dashboard-tour img").evaluate_all(
                                "images => Promise.all(images.map(image => { image.loading='eager'; return image.decode(); }))")
                            page.screenshot(path=str(screenshots / f"dashboard-{width}-{theme}.png"), full_page=True)
            assert not animations, "Animations must not be requested before play"
            page.set_viewport_size({"width": 1440, "height": 1000})
            page.goto(base + "/dashboard.html")
            for attrs in buttons:
                button = page.locator(f'[aria-controls="{attrs["aria-controls"]}"]')
                image = page.locator("#" + attrs["aria-controls"])
                button.focus()
                page.keyboard.press("Enter")
                expect(image).to_have_attribute("src", attrs["data-tour"])
                expect(button).to_have_attribute("aria-pressed", "true")
                expect(button.locator("..").locator(".tour-status")).to_contain_text("Playing once")
                button.click()
                expect(image).to_have_attribute("src", attrs["data-poster"])
            first = page.locator("[data-tour]").first
            first.click()
            page.locator("#start").scroll_into_view_if_needed()
            expect(first).to_have_attribute("aria-pressed", "false")
            first.click()
            page.emulate_media(reduced_motion="reduce")
            for button in page.locator("[data-tour]").all():
                expect(button).to_be_disabled()
                expect(button).to_have_attribute("aria-pressed", "false")
            page.emulate_media(reduced_motion="no-preference")
            first.click()
            expect(first.locator("..").locator(".tour-status")).to_contain_text("Playing once")
            expect(first).to_have_attribute("aria-pressed", "false", timeout=21000)
            expect(first.locator("..").locator(".tour-status")).to_contain_text("Tour finished")
            page.goto(base + "/")
            for tab in page.get_by_role("tab").all():
                tab.click()
                expect(page.locator("#" + tab.get_attribute("aria-controls"))).to_be_visible()
            assert not errors, errors
            fallback = browser.new_context(java_script_enabled=False)
            static = fallback.new_page()
            static.goto(base + "/dashboard.html")
            for attrs in buttons:
                expect(static.locator("#" + attrs["aria-controls"])).to_have_attribute("src", attrs["data-poster"])
            expect(static.locator("[data-tour]:visible")).to_have_count(0)
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    print("Dashboard APNG, controls, motion, fallback, navigation and responsive checks passed.")


if __name__ == "__main__":
    check(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else None)
