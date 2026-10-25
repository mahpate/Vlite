"""Capture disconnected UI for diagnostics, not the public guided tour.

Usage: python scripts/capture_dashboard_screenshots.py <output-directory> [dashboard-url]
Requires the existing tests/requirements-dashboard.txt browser tooling.
Start the current dashboard container first. No API requests or writes are allowed.
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
            requests.append(route.request.url)
            route.abort()

        context.route("**/api/**", block_api)
        page = context.new_page()
        errors = []
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.goto(base)
        page.locator("#tour-dismiss").click()
        expect(page.locator("#connection-badge")).to_have_text("Disconnected")
        expect(page.locator("#token")).to_have_value("")
        preferences = page.locator(".topbar .appearance-settings")

        def save(name):
            page.evaluate("window.scrollTo(0, 0)")
            expect(page.locator("#token")).to_have_value("")
            page.screenshot(path=str(output / name), animations="disabled")

        for theme in ("light", "dark"):
            preferences.locator("summary").click()
            page.locator("#theme").select_option(theme)
            preferences.locator("summary").click()
            save(f"current-overview-{theme}.png")
            if theme == "light":
                page.locator(".token-help summary").click()
                expect(page.locator("#token-hint")).to_be_visible()
                save("current-token-help.png")
                page.locator(".token-help summary").click()
            else:
                preferences.locator("summary").click()
                expect(page.locator("#theme")).to_be_visible()
                save("current-preferences.png")
        assert not requests, "Disconnected capture must not request API data"
        assert not errors, errors
        context.close()
        browser.close()
    print("Captured four current dashboard screenshots; disconnected, no credentials or API data.")


if __name__ == "__main__":
    if len(sys.argv) not in (2, 3):
        raise SystemExit(__doc__)
    capture(Path(sys.argv[1]).resolve(), sys.argv[2] if len(sys.argv) > 2 else "http://127.0.0.1:8081")
