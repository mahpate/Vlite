"""Verify the same accessible API browser for every source-checked interface."""
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import sys
from threading import Thread
from urllib.parse import quote

from playwright.sync_api import expect, sync_playwright

from api_reference import ROOT, canonical_rest_key, source_keys


class QuietHandler(SimpleHTTPRequestHandler):
    def log_message(self, *_):
        pass


def check(root, screenshots=None):
    expected = source_keys()
    families = {"rest": "REST", "node": "Node.js", "rust": "Rust", "c": "C ABI",
                "py": "Python", "grpc": "gRPC", "mcp": "MCP", "wasm": "WASM"}
    server = ThreadingHTTPServer(("127.0.0.1", 0), partial(QuietHandler, directory=str(root)))
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base = f"http://127.0.0.1:{server.server_port}/api.html"
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch()
            context = browser.new_context(permissions=["clipboard-read", "clipboard-write"])
            page = context.new_page()
            errors, external = [], []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.on("request", lambda request: external.append(request.url) if not request.url.startswith(base.rsplit("/", 1)[0]) else None)
            page.goto(base)
            expect(page.locator("#api-browser")).to_be_visible()
            for family, surface in families.items():
                page.locator(f'[data-family="{family}"]').click()
                expect(page.locator(f'[data-family="{family}"]')).to_have_attribute("aria-pressed", "true")
                links = page.locator("#api-results a")
                expect(links).to_have_count(len(expected[surface]))
                keys = set(links.evaluate_all("links => links.map(link => link.dataset.key)"))
                # The printable OpenAPI-era keys use {c}; route comparison canonicalizes them.
                assert {canonical_rest_key(key) for key in keys} == expected[surface]
                links.last.click()
                expect(page.locator("#api-detail")).to_have_attribute("data-key", links.last.get_attribute("data-key"))
                for heading in ("Signature", "Behavior", "Access and execution"):
                    expect(page.locator("#api-detail").get_by_role("heading", name=heading, exact=True)).to_be_visible()
                assert page.locator("#api-detail .api-behavior").inner_text().strip()
                for theme in ("light", "dark"):
                    page.evaluate("theme => document.documentElement.dataset.theme = theme", theme)
                    for width in (1440, 768, 390, 320):
                        page.set_viewport_size({"width": width, "height": 1000})
                        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (family, theme, width)
                page.set_viewport_size({"width": 1440, "height": 1000})
            page.locator('[data-family="rest"]').click()
            page.locator("#api-search").fill("POST /v1/index/hnsw/quality")
            expect(page.locator("#api-results a")).to_have_count(1)
            expect(page.locator("#api-detail-title")).to_have_text("POST /v1/index/hnsw/quality")
            page.get_by_role("button", name="Copy signature", exact=True).click()
            expect(page.get_by_role("button", name="Copied", exact=True)).to_be_visible()
            assert page.evaluate("navigator.clipboard.readText()") == "POST /v1/index/hnsw/quality"
            page.locator("#api-results a").click()
            page.reload()
            expect(page.locator("#api-detail-title")).to_have_text("POST /v1/index/hnsw/quality")
            page.locator("#api-category").select_option("Dashboard assets")
            expect(page.locator("#api-results a")).to_have_count(18)
            page.locator("#api-search").fill("no-such-call")
            expect(page.locator("#api-results a")).to_have_count(0)
            expect(page.locator("#api-detail-title")).to_have_text("No matching calls")
            page.get_by_role("button", name="Clear filters", exact=True).click()
            expect(page.locator("#api-search")).to_be_focused()
            expect(page.locator("#api-results a")).to_have_count(len(expected["REST"]))
            page.goto(base + "#api=" + quote("py:Database.search", safe=""))
            expect(page.locator("#api-detail-title")).to_have_text("Database.search")
            page.locator("#api-detail").get_by_role("link", name="Full section", exact=True).click()
            expect(page.locator("#sdk-search")).to_be_visible()
            assert page.locator(".api-full-reference").evaluate("element => element.open")
            page.go_back()
            expect(page.locator("#api-detail-title")).to_have_text("Database.search")
            page.goto(base + "#rest-indexes")
            expect(page.locator("#rest-indexes")).to_be_in_viewport()
            page.goto(base + "#api=%broken")
            expect(page.locator("#api-count")).to_contain_text("not available")
            page.locator('.api-family-cards [data-api-family="node"]').click()
            expect(page.locator('[data-family="node"]')).to_be_focused()
            expect(page.locator("#api-detail .kicker")).to_contain_text("Node.js")
            page.locator("#api-results a").first.focus()
            page.keyboard.press("Enter")
            expect(page.locator("#api-detail")).to_be_focused()
            page.locator('.docs-sidebar [data-api-family="grpc"]').click()
            expect(page.locator('[data-family="grpc"]')).to_have_attribute("aria-pressed", "true")
            page.evaluate("window.dispatchEvent(new Event('beforeprint'))")
            assert page.locator(".api-full-reference").evaluate("element => element.open")
            page.evaluate("window.dispatchEvent(new Event('afterprint'))")
            if screenshots:
                screenshots.mkdir(parents=True, exist_ok=True)
                for theme in ("light", "dark"):
                    page.goto(base + "#api=" + quote("rest:POST /v1/index/hnsw/quality", safe=""))
                    page.evaluate("theme => document.documentElement.dataset.theme = theme", theme)
                    for width in (1440, 390):
                        page.set_viewport_size({"width": width, "height": 1000})
                        page.locator("#api-browser").evaluate("element => element.scrollIntoView({block: 'start'})")
                        page.screenshot(path=str(screenshots / f"api-{theme}-{width}.png"))
            assert not errors, errors
            assert not external, external
            context.close()
            static = browser.new_context(java_script_enabled=False, viewport={"width": 390, "height": 900})
            page = static.new_page()
            page.goto(base + "#rest-indexes")
            expect(page.locator("#rest-indexes")).to_be_visible()
            expect(page.locator("#api-browser")).to_be_hidden()
            expect(page.locator('[data-api="rest:POST /v1/index/hnsw/quality"]')).to_be_visible()
            assert page.locator("[data-api]").count() == sum(map(len, expected.values()))
            assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
            static.close()
            browser.close()
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    print("All eight API families: complete inventory, shared details, search, areas, deep links, keyboard, copy, light/dark, four widths and no-JS passed.")


if __name__ == "__main__":
    check(Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "site",
          Path(sys.argv[2]) if len(sys.argv) > 2 else None)
