"""Check the current screenshot tour, autoplay, controls and accessible fallbacks.

Usage: python tests/dashboard_tours.py <assembled-site>
Requires tests/requirements-dashboard.txt.
"""

from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import hashlib
import json
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
    frames = (
        "current-overview-light.png", "current-sign-in.png", "current-records.png",
        "current-payload.png", "current-metadata.png", "current-vector.png",
        "current-write.png", "current-import.png", "current-index.png", "current-collections.png",
        "current-search.png", "current-hybrid.png", "current-retrieval.png", "current-explore.png",
        "current-graph.png", "current-graph-edit.png", "current-observability.png", "current-feed.png", "current-administration.png",
        "current-users.png", "current-credentials.png", "current-tenants.png",
        "current-backups.png", "current-replication.png", "current-automation.png",
        "current-webhooks.png", "current-tls.png", "current-developer.png",
        "current-preferences.png",
    )
    assets = root / "assets" / "dashboard"
    manifest = json.loads((assets / "connected-captures.json").read_text())
    assert manifest["connected"] and manifest["record_count"] >= 8
    assert manifest["format_version"] >= 5
    assert manifest["theme_sha256"] == hashlib.sha256(
        (root / "assets" / "theme.css").read_text(encoding="utf-8").encode()).hexdigest(), "Recapture after changing the shared theme"
    assert set(manifest["shared_assets"]) == {
        "fonts/outfit-latin-wght-normal.woff2", "fonts/jetbrains-mono-latin-wght-normal.woff2",
        "vendor/three/three.min.js",
    }
    for name, checksum in manifest["shared_assets"].items():
        file = root / "assets" / name
        content = file.read_bytes() if name.endswith(".woff2") else file.read_text(encoding="utf-8").encode()
        assert hashlib.sha256(content).hexdigest() == checksum, (
            f"Recapture after changing shared dashboard asset {name}")
    assert {capture["workspace"] for capture in manifest["captures"]} == {
        "Sign in", "Overview", "Collections", "Search", "Graph", "Observability", "Administration", "Developer Console",
    }
    source = Path(__file__).resolve().parents[1] / "src" / "dashboard"
    for filename, checksum in manifest["source_assets"].items():
        assert hashlib.sha256((source / filename).read_text(encoding="utf-8").encode()).hexdigest() == checksum, (
            f"Recapture the connected tour after changing dashboard asset {filename}")
    captures = {capture["file"]: capture for capture in manifest["captures"]}
    for frame in frames:
        assert hashlib.sha256((assets / frame).read_bytes()).hexdigest() == captures[frame]["sha256"]
    assert len({captures[frame]["sha256"] for frame in frames}) == len(frames)
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
                expect(image).to_have_attribute("src", "assets/dashboard/" + frames[1] + "?v=connected", timeout=5000)
                toggle.click()
                expect(toggle).to_have_text("Play tour")
                paused = image.get_attribute("src")
                page.wait_for_timeout(1700)
                expect(image).to_have_attribute("src", paused)
                page.locator("[data-tour-next]").click()
                assert image.get_attribute("src") != paused
                expect(page.locator("[data-tour-step] option")).to_have_count(len(frames))
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
                if width == 1440:
                    assert page.locator("[data-guided-tour]").count() == 1
                    assert page.locator(".dashboard-tours").count() == 0
                    for number, frame in enumerate(frames):
                        page.locator("[data-tour-step]").select_option(str(number))
                        expect(toggle).to_have_text("Play tour")
                        expect(image).to_have_attribute("src", "assets/dashboard/" + frame + "?v=connected")
                        page.wait_for_function("img => img.complete && img.naturalWidth === 1440",
                                               arg=image.element_handle())
                        expect(page.locator("[data-tour-position]")).to_have_text(f"{number + 1} / {len(frames)}")
                        assert page.locator("[data-tour-full]").get_attribute("href").endswith(frame + "?v=connected")
                page.goto(base + "/docs/CONSOLE.html")
                for screenshot in page.locator(".docs-main img").all():
                    screenshot.scroll_into_view_if_needed()
                    page.wait_for_function("img => img.complete && img.naturalWidth > 0",
                                           arg=screenshot.element_handle())
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
                "src", "assets/dashboard/" + frames[0] + "?v=connected")
            expect(static.locator(".tour-controls")).to_be_hidden()
            fallback.close()
            # The flow is a self-contained SVG: no script or external asset is needed.
            flow = (root / "assets" / "dashboard-flow.svg").read_text(encoding="utf-8")
            assert "<script" not in flow and "href=" not in flow
            page.goto(base + "/assets/dashboard-flow.svg")
            assert page.locator("svg").get_attribute("aria-labelledby") == "title description"
            for scheme, background in (("light", "rgb(247, 248, 252)"), ("dark", "rgb(12, 16, 27)")):
                page.emulate_media(color_scheme=scheme, reduced_motion="no-preference")
                assert page.locator("svg > rect").first.evaluate("el => getComputedStyle(el).fill") == background
                assert page.locator(".pulse").first.evaluate("el => getComputedStyle(el).animationName") == "flow"
                assert page.locator(".pulse").first.evaluate("el => getComputedStyle(el).animationIterationCount") == "3"
                page.emulate_media(reduced_motion="reduce")
                assert page.locator(".pulse").first.evaluate("el => getComputedStyle(el).animationName") == "none"
            for width in (1440, 390, 320):
                page.set_viewport_size({"width": width, "height": 1000})
                page.goto(base + "/dashboard.html")
                page.get_by_text("Request flow", exact=True).click()
                expect(page.locator('img[src="assets/dashboard-flow.svg"]')).to_be_visible()
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
            assert not errors, errors
            context.close()
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    print(f"Connected {len(frames)}-step tour: sign-in screen, seven workspaces, unique verified captures, autoplay, step selection, pause, motion, offscreen, no-JS and guide images passed.")


if __name__ == "__main__":
    check(Path(sys.argv[1]).resolve())
