"""Dashboard presentation checks using the existing explicit API fixture.

Usage: python tests/dashboard_design.py [screenshot-directory]
Requires tests/requirements-dashboard.txt. Images are UI fixtures, not live data
or performance evidence. Functional server behavior is covered by dashboard_e2e.
"""

from pathlib import Path
import sys

from dashboard_e2e import FakeApi, MOCK_ORIGIN, expect_signed_out, sign_in_with_token, sign_out
from playwright.sync_api import expect, sync_playwright


CONTRAST = """element => {
    const parse = color => color.match(/[\\d.]+/g).slice(0, 3).map(Number);
    const luminance = color => parse(color).map(v => {
        v /= 255; return v <= .04045 ? v / 12.92 : ((v + .055) / 1.055) ** 2.4;
    }).reduce((sum, v, i) => sum + v * [.2126, .7152, .0722][i], 0);
    const style = getComputedStyle(element);
    let background = style.backgroundColor;
    for (let parent = element.parentElement; background === 'rgba(0, 0, 0, 0)' && parent; parent = parent.parentElement) {
        background = getComputedStyle(parent).backgroundColor;
    }
    const a = luminance(style.color), b = luminance(background);
    return (Math.max(a, b) + .05) / (Math.min(a, b) + .05);
}"""


def check(screenshots=None):
    with sync_playwright() as p:
        browser = p.chromium.launch()
        for theme in ("light", "dark"):
            fake = FakeApi()
            context = browser.new_context(viewport={"width": 1440, "height": 1050}, color_scheme=theme,
                                          reduced_motion="reduce")
            context.route(MOCK_ORIGIN + "/**", fake.handle)
            page = context.new_page()
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.goto(MOCK_ORIGIN + "/dashboard")
            # Only the sign-in screen shows until a credential is verified.
            expect_signed_out(page)
            for selector in ("#gate-title", "#gate-intro", "#token-sign-in summary", "#token-sign-in label", ".gate-footer"):
                value = page.locator(selector).first.evaluate(CONTRAST)
                assert value >= 4.5, (theme, "sign-in", selector, value)
            for width, height in ((1440, 1050), (768, 900), (390, 844), (320, 740), (844, 390)):
                page.set_viewport_size({"width": width, "height": height})
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (theme, "sign-in", width)
                for button in page.locator("#sign-in-gate button:visible").all():
                    assert button.bounding_box()["height"] >= 44, (theme, "sign-in", width, button.inner_text())
                if screenshots and width in (1440, 390):
                    screenshots.mkdir(parents=True, exist_ok=True)
                    page.screenshot(path=str(screenshots / f"studio-sign-in-{theme}-{width}.png"))
            page.set_viewport_size({"width": 1440, "height": 1050})
            page.locator(".token-help summary").focus()
            page.keyboard.press("Enter")
            expect(page.locator("#token-hint")).to_be_visible()
            expect(page.locator(".token-help")).to_contain_text("VLITE_TOKEN_FILE")
            expect(page.locator(".token-help")).to_contain_text("without the Bearer prefix")
            page.locator(".token-help summary").press("Enter")
            sign_in_with_token(page, "admin-" + "t" * 32)
            expect(page.locator("#connection-badge")).to_have_text("Connected")
            page.locator("#tour-dismiss").click()
            for selector in (".sidebar nav a[aria-current]", ".sidebar nav a:not([aria-current])", ".nav-caption",
                             "#workspace-description", ".stat-grid span", "#connection-badge", "#identity-badge", "#status"):
                value = page.locator(selector).first.evaluate(CONTRAST)
                assert value >= 4.5, (theme, selector, value)
            assert page.locator(".sidebar nav svg[aria-hidden=true]").count() == 7
            for width, height in ((1440, 1050), (1024, 900), (768, 900), (390, 844), (320, 740), (844, 390)):
                page.set_viewport_size({"width": width, "height": height})
                for route, section in (("database", "database"), ("browse", "browse"), ("search", "search"),
                                       ("observability", "observability"), ("operations", "operations"),
                                       ("requests", "requests")):
                    page.evaluate("target => window.VLiteNavigation.reveal(target)", route)
                    expect(page.locator(f"#{section}")).to_be_visible()
                    menu = page.locator(".topbar #user-menu")
                    expect(menu.locator("summary")).to_be_visible()
                    menu.evaluate("element => element.open = true")
                    expect(menu.locator("#theme")).to_be_visible()
                    assert page.locator("#theme").bounding_box()["height"] >= 44
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (theme, route, width, "account menu")
                    panel = menu.locator(".user-menu-panel").bounding_box()
                    assert panel["x"] >= 0 and panel["x"] + panel["width"] <= width, (theme, route, width, panel)
                    menu.evaluate("element => element.open = false")
                    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (theme, route, width)
                    for button in page.locator(f"#{section} button:visible").all():
                        assert button.bounding_box()["height"] >= 44, (theme, route, button.inner_text())
                    if screenshots and width in (1440, 390) and route in ("database", "browse", "search"):
                        screenshots.mkdir(parents=True, exist_ok=True)
                        page.evaluate("window.scrollTo(0, 0)")
                        page.screenshot(path=str(screenshots / f"studio-{route}-{theme}-{width}.png"))
            page.set_viewport_size({"width": 1440, "height": 1050})
            page.evaluate("window.VLiteNavigation.reveal('database')")
            page.evaluate("window.scrollTo(0, document.documentElement.scrollHeight)")
            assert abs(page.locator(".topbar").bounding_box()["y"]) <= 1
            menu = page.locator("#user-menu")
            menu.locator("summary").focus()
            page.keyboard.press("Enter")
            expect(page.locator("#theme")).to_be_visible()
            page.locator("#theme").select_option("dark" if theme == "light" else "light")
            expect(page.locator("html")).to_have_attribute("data-theme", "dark" if theme == "light" else "light")
            page.keyboard.press("Escape")
            expect(page.locator("#theme")).to_be_hidden()
            expect(menu.locator("summary")).to_be_focused()
            for title, target in (("Server compatibility", "#compatibility"),
                                  ("Database details and metrics (JSON)", "#overview")):
                details = page.locator(target).locator("..")
                details.evaluate("element => element.open = false")
                details.locator("summary").focus()
                page.keyboard.press("Enter")
                expect(page.locator(target)).to_be_visible()
            sign_out(page)
            expect(page.locator("#connection-badge")).to_have_text("Disconnected")
            assert not errors, errors
            context.close()
        browser.close()
    print("Checked modern dashboard: sign-in screen, seven workspaces, account menu, both themes, text contrast, touch targets, narrow/landscape layouts and keyboard disclosures.")


if __name__ == "__main__":
    check(Path(sys.argv[1]) if len(sys.argv) > 1 else None)
