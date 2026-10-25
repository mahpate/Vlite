"""Check the current screenshot tour, autoplay, controls and accessible fallbacks.

Usage: python tests/dashboard_tours.py <assembled-site>
Requires tests/requirements-dashboard.txt.
"""

from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import sys
from threading import Thread

from playwright.sync_api import expect, sync_playwright


class QuietHandler(SimpleHTTPRequestHandler):
    def log_message(self, *_):
        pass


def check(root):
    html = (root / "dashboard.html").read_text(encoding="utf-8")
    assert "Earlier release tours" not in html
    assert "records-tour" not in html and "search-tour" not in html
    frames = ("current-overview-light.png", "current-token-help.png",
              "current-overview-dark.png", "current-preferences.png")
    for frame in frames:
        assert (root / "assets" / "dashboard" / frame).is_file()
    server = ThreadingHTTPServer(("127.0.0.1", 0), partial(QuietHandler, directory=str(root)))
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch()
            context = browser.new_context(reduced_motion="no-preference")
            page = context.new_page()
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            base = f"http://127.0.0.1:{server.server_port}"
            for width in (1440, 900, 390, 320):
                page.set_viewport_size({"width": width, "height": 1000})
                page.goto(base + "/dashboard.html")
                image = page.locator("#dashboard-guided-tour")
                toggle = page.locator("[data-tour-toggle]")
                image.scroll_into_view_if_needed()
                expect(toggle).to_have_text("Pause tour")
                expect(image).to_have_attribute("src", "assets/dashboard/" + frames[1], timeout=5000)
                toggle.click()
                expect(toggle).to_have_text("Play tour")
                paused = image.get_attribute("src")
                page.wait_for_timeout(1700)
                expect(image).to_have_attribute("src", paused)
                page.locator("[data-tour-next]").click()
                assert image.get_attribute("src") != paused
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
                page.goto(base + "/docs/CONSOLE.html")
                for screenshot in page.locator(".docs-main img").all():
                    screenshot.scroll_into_view_if_needed()
                    screenshot.evaluate("img => img.decode()")
                    assert screenshot.evaluate("img => img.naturalWidth === 1440")
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
            page.goto(base + "/dashboard.html")
            image = page.locator("#dashboard-guided-tour")
            toggle = page.locator("[data-tour-toggle]")
            image.scroll_into_view_if_needed()
            page.emulate_media(reduced_motion="reduce")
            expect(toggle).to_have_text("Play tour")
            paused = image.get_attribute("src")
            page.wait_for_timeout(1700)
            expect(image).to_have_attribute("src", paused)
            toggle.focus()
            page.keyboard.press("Enter")
            expect(toggle).to_have_text("Pause tour")
            expect(image).not_to_have_attribute("src", paused, timeout=5000)
            toggle.click()
            image.scroll_into_view_if_needed()
            page.locator("[data-tour-next]").click()
            expect(toggle).to_have_text("Play tour")
            page.emulate_media(reduced_motion="no-preference")
            page.locator("#start").scroll_into_view_if_needed()
            page.wait_for_timeout(300)
            paused = image.get_attribute("src")
            page.wait_for_timeout(1700)
            expect(image).to_have_attribute("src", paused)
            assert not errors, errors
            fallback = browser.new_context(java_script_enabled=False)
            static = fallback.new_page()
            static.goto(base + "/dashboard.html")
            expect(static.locator("#dashboard-guided-tour")).to_have_attribute(
                "src", "assets/dashboard/" + frames[0])
            expect(static.locator(".tour-controls")).to_be_hidden()
            fallback.close()
            context.close()
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    print("Current screenshot tour: autoplay, pause, next, motion, offscreen, no-JS and guide images passed.")


if __name__ == "__main__":
    check(Path(sys.argv[1]).resolve())
