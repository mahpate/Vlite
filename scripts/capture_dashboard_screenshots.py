"""Capture the disconnected sign-in screen for diagnostics, not the public guided tour.

Usage: python scripts/capture_dashboard_screenshots.py <output-directory> [dashboard-url]
Requires the existing tests/requirements-dashboard.txt browser tooling.
Start the current dashboard container first. Every API request is blocked, so the page
stays on its sign-in screen with token sign-in only. No credentials are used or written.
For website images, use the hosted dashboard-capture.yml connected workflow.
"""

from pathlib import Path
import sys

from playwright.sync_api import expect, sync_playwright


def capture(output, base):
    website = Path(__file__).resolve().parents[1] / "site"
    if output == website or website in output.parents:
        raise ValueError("Disconnected diagnostics must not replace connected website screenshots")
    output.mkdir(parents=True, exist_ok=True)
    with sync_playwright() as p:
        browser = p.chromium.launch()
        context = browser.new_context(
            viewport={"width": 1440, "height": 1100},
            device_scale_factor=1, reduced_motion="reduce",
        )
        requests = []

        def block_api(route):
            request = route.request
            requests.append((request.method, request.url, "authorization" in request.headers))
            route.abort()

        context.route("**/api/**", block_api)
        page = context.new_page()
        errors = []
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.goto(base)
        # Only the sign-in screen renders until a credential is verified.
        expect(page.locator("#sign-in-gate")).to_be_visible()
        expect(page.locator("#app-shell")).to_be_hidden()
        expect(page.locator("#token")).to_be_visible()

        def save(name):
            page.evaluate("window.scrollTo(0, 0)")
            expect(page.locator("#token")).to_have_value("")
            page.screenshot(path=str(output / name), animations="disabled")

        for theme in ("light", "dark"):
            page.emulate_media(color_scheme=theme)
            expect(page.locator("html")).to_have_attribute("data-theme", theme)
            save(f"current-sign-in-{theme}.png")
        page.emulate_media(color_scheme="light")
        page.locator(".token-help summary").click()
        expect(page.locator("#token-hint")).to_be_visible()
        save("current-token-help.png")
        # Loading the page may probe the unauthenticated sign-in options; nothing else is requested.
        unexpected = [(method, url) for method, url, authorized in requests
                      if method != "GET" or not url.endswith("/v1/auth/config") or authorized]
        assert not unexpected, f"Disconnected capture must not request API data: {unexpected}"
        assert not errors, errors
        context.close()
        browser.close()
    print("Captured three disconnected sign-in screenshots; no credentials or API data.")


if __name__ == "__main__":
    if len(sys.argv) not in (2, 3):
        raise SystemExit(__doc__)
    capture(Path(sys.argv[1]).resolve(), sys.argv[2] if len(sys.argv) > 2 else "http://127.0.0.1:8081")
