"""Real Chromium UI tests against an isolated Docker API; keeps its named data volume."""

import json
import os
import re
import secrets
import subprocess
import tempfile
import time
import uuid
from playwright.sync_api import Error as PlaywrightError, sync_playwright, expect

expect.set_options(timeout=30000)


def reveal_control(locator):
    """Navigate to an existing control without unhiding gated or secret containers."""
    locator.evaluate_all("""elements => {
            const element = elements[0];
            if (!element) return;
            const section = element.closest("main > section");
            if (section && section.hidden) window.VLiteNavigation.reveal(section.id);
            for (let parent = element.parentElement; parent; parent = parent.parentElement) {
                if (parent.tagName === "DETAILS") parent.open = true;
            }
        }""")
    return locator


def control(page, selector):
    return reveal_control(page.locator(selector))


def role(page, role_name, **kwargs):
    reveal_control(page.get_by_role(role_name, **{**kwargs, "include_hidden": True}))
    return page.get_by_role(role_name, **kwargs)


def assert_no_stored_credentials(page, token):
    stored = page.evaluate("""() => ({
        local: Object.fromEntries(Object.entries(localStorage)),
        session: Object.fromEntries(Object.entries(sessionStorage))
    })""")
    assert stored["session"] == {}, stored
    assert set(stored["local"]) <= {"vlite-studio-tour"}, stored
    if "vlite-studio-tour" in stored["local"]:
        assert stored["local"]["vlite-studio-tour"] == "dismissed", stored
    assert token not in json.dumps(stored), "Bearer token persisted in browser storage"


def docker(*args, **kwargs):
    result = subprocess.run(["docker", *args], capture_output=True, text=True, timeout=600, **kwargs)
    if result.returncode:
        raise RuntimeError(f"Docker {args[0]} failed ({result.returncode}):\n{result.stdout[-12000:]}\n{result.stderr[-12000:]}")
    return result.stdout.strip()

def ready(page, base):
    deadline = time.monotonic() + 120
    last_error = None
    while time.monotonic() < deadline:
        try:
            response = page.request.get(base + "/health", timeout=10000)
            if response.status == 200:
                return
        except PlaywrightError as error:
            last_error = error
        time.sleep(.1)
    raise RuntimeError("Dashboard API did not become ready within 120 seconds") from last_error


def exercise_layout(page, url):
    page.goto(url)
    expect(page.locator("main > section:visible")).to_have_count(1)
    expect(page.locator("#database")).to_be_visible()
    expect(page.locator("#search")).to_be_hidden()
    nav = page.get_by_role("navigation", name="Dashboard navigation")
    expect(nav.get_by_role("link")).to_have_text([
        "Overview", "Collections", "Search", "Observability", "Administration", "Developer Console",
    ])
    for width in [1280, 768, 390]:
        page.set_viewport_size({"width": width, "height": 844})
        page.evaluate("window.scrollTo(0, 0)")
        toggle = page.get_by_role("button", name="☰ Menu", exact=True)
        if width == 390:
            expect(nav).to_be_hidden()
            toggle.click()
            expect(toggle).to_have_attribute("aria-expanded", "true")
            expect(page.get_by_role("dialog", name="Workspace navigation")).to_be_visible()
            expect(nav.get_by_role("link", name="Overview", exact=True)).to_be_focused()
            page.keyboard.press("Escape")
            expect(toggle).to_have_attribute("aria-expanded", "false")
            expect(toggle).to_be_focused()
            toggle.click()
            # Tab and Shift+Tab stay inside the modal drawer.
            focusable = page.locator("#dashboard-sidebar").locator(
                "a[href]:visible, button:not([disabled]):visible, "
                "select:not([disabled]):visible, input:not([disabled]):visible"
            )
            focusable.first.focus()
            page.keyboard.press("Shift+Tab")
            expect(focusable.last).to_be_focused()
            page.keyboard.press("Tab")
            expect(focusable.first).to_be_focused()
        else:
            expect(toggle).to_be_hidden()
            expect(nav).to_be_visible()
        nav.get_by_role("link", name="Administration", exact=True).click()
        expect(page).to_have_url(url + "#operations")
        expect(page.locator("#workspace-title")).to_have_text("Administration")
        expect(page.locator("main > section:visible")).to_have_count(1)
        expect(page.locator("#operations")).to_be_visible()
        expect(page.locator("#database")).to_be_hidden()
        expect(page.locator('nav [aria-current="page"]')).to_have_text("Administration")
        box = page.locator("#operations").bounding_box()
        assert box["width"] >= min(280, width - 40), (width, box)
        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
        if width == 390:
            expect(toggle).to_have_attribute("aria-expanded", "false")
            expect(page.get_by_role("heading", name="Health and administration", exact=True)).to_be_focused()
            toggle.click()
        nav.get_by_role("link", name="Overview", exact=True).click()
        expect(page).to_have_url(url + "#database")
    page.set_viewport_size({"width": 1280, "height": 844})
    # Legacy deep links and real history restore the correct focused workspace.
    for target, title in [("browse", "Collections"), ("editor", "Collections"), ("operations", "Administration")]:
        page.goto(url + "#" + target)
        expect(page.locator("main > section:visible")).to_have_count(1)
        expect(page.locator("#" + target)).to_be_visible()
        expect(page.locator("#workspace-title")).to_have_text(title)
    nav.get_by_role("link", name="Search", exact=True).click()
    page.locator("#query").fill("[0.25,0.5,0.75]")
    nav.get_by_role("link", name="Collections", exact=True).click()
    page.get_by_role("tab", name="Write", exact=True).click()
    page.locator("#metadata").fill('{"draft":{"type":"string","value":"not saved"}}')
    nav.get_by_role("link", name="Overview", exact=True).click()
    page.go_back()
    expect(page.locator("#editor")).to_be_visible()
    expect(page.locator("#metadata")).to_have_value('{"draft":{"type":"string","value":"not saved"}}')
    page.go_forward()
    expect(page.locator("#database")).to_be_visible()
    nav.get_by_role("link", name="Search", exact=True).click()
    expect(page.locator("#query")).to_have_value("[0.25,0.5,0.75]")
    nav.get_by_role("link", name="Collections", exact=True).click()
    tabs = page.get_by_role("tablist", name="Collections views")
    tabs.get_by_role("tab", name="Records / Browse", exact=True).focus()
    page.keyboard.press("ArrowRight")
    expect(tabs.get_by_role("tab", name="Write", exact=True)).to_be_focused()
    expect(tabs.get_by_role("tab", name="Write", exact=True)).to_have_attribute("aria-selected", "true")
    expect(page.locator("#editor")).to_be_visible()
    page.keyboard.press("End")
    expect(tabs.get_by_role("tab", name="Settings", exact=True)).to_be_focused()
    expect(page.locator("#collections")).to_be_visible()
    page.keyboard.press("Home")
    expect(tabs.get_by_role("tab", name="Records / Browse", exact=True)).to_be_focused()
    expect(page.locator("#browse")).to_be_visible()
    expect(page.locator("main > section:visible")).to_have_count(1)
    page.goto(url)


def exercise_observability(page, vectors):
    control(page, "#observability").scroll_into_view_if_needed()
    control(page, "#obs-interval").select_option("0")
    control(page, "#obs-refresh").click()
    expect(control(page, "#obs-metrics-status")).to_contain_text("Live metrics")
    expect(control(page, "#obs-vectors")).to_have_text(str(vectors))
    expect(control(page, "#obs-logs-status")).to_contain_text("Tailing server log ring")
    expect(control(page, "#obs-log-view details").first).to_be_visible()


def exercise(page, base, token):
    errors = []
    assets = {}
    page.on("pageerror", lambda error: errors.append(str(error)))
    page.on("response", lambda response: assets.update({response.url: response.status}) if "/dashboard/" in response.url else None)
    page.goto(base + "/dashboard")
    assert assets.get(base + "/dashboard/app.js") == 200, assets
    assert assets.get(base + "/dashboard/navigation.js") == 200, assets
    assert assets.get(base + "/dashboard/observability.js") == 200, assets
    assert assets.get(base + "/dashboard/style.css") == 200, assets
    assert not errors, errors
    expect(role(page, "heading", name="Connection", exact=True)).to_be_visible()
    expect(control(page, "#status")).to_have_text("Disconnected")
    control(page, "#token").fill("x" * 32)
    role(page, "button", name="Connect / refresh").click()
    expect(control(page, "#status")).to_contain_text("HTTP 401")
    control(page, "#token").fill(token)
    role(page, "button", name="Connect / refresh").click()
    expect(control(page, "#status")).to_have_text("Connected; overview refreshed")
    expect(control(page, "#token")).to_have_value("")
    expect(control(page, "#overview")).to_contain_text('"dimensions": 3')
    expect(control(page, "#connection-badge")).to_have_text("Connected")
    expect(control(page, "#stat-records")).to_have_text("0")
    expect(control(page, "#stat-dimensions")).to_have_text("3")
    exercise_observability(page, 0)
    role(page, "button", name="Fill example", exact=True).click()
    expect(control(page, "#record-vector")).to_have_value("[1,0,0]")
    assert json.loads(control(page, "#metadata").input_value())["category"]["value"] == "demo"
    expect(control(page, "#status")).to_contain_text("nothing saved")
    role(page, "button", name="Use editor vector as query", exact=True).click()
    expect(control(page, "#query")).to_have_value("[1,0,0]")
    expect(control(page, "#index-status")).to_contain_text("Not ready")
    role(page, "button", name="First page / refresh", exact=True).click()
    expect(control(page, "#browse-status")).to_contain_text("0 records")
    expect(control(page, "#next-page")).to_be_disabled()
    role(page, "button", name="Connect / refresh").click()
    expect(control(page, "#status")).to_have_text("Connected; overview refreshed")
    control(page, "#id").fill("18446744073709551615")
    control(page, "#record-vector").fill("[1,0,0]")
    control(page, "#payload").fill("AP8=")
    # Untrusted data must be rendered as text, never injected HTML.
    metadata = {"category": {"type": "string", "value": "<img src=x onerror=alert(1)>"}}
    control(page, "#metadata").fill(json.dumps(metadata))
    role(page, "button", name="Put / replace").click()
    expect(control(page, "#confirmation")).to_be_visible()
    role(page, "button", name="Cancel", exact=True).click()
    role(page, "button", name="Get", exact=True).click()
    expect(control(page, "#status")).to_contain_text("HTTP 404")
    role(page, "button", name="Put / replace").click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#record")).to_contain_text('"committed": true')
    expect(control(page, "#stat-records")).to_have_text("--")
    role(page, "button", name="First page / refresh", exact=True).click()
    expect(control(page, "#vectors tbody tr")).to_have_count(1)
    expect(control(page, "#vectors tbody")).to_contain_text("<img src=x onerror=alert(1)>")
    expect(control(page, "#vectors img")).to_have_count(0)
    role(page, "button", name="18446744073709551615", exact=True).click()
    expect(control(page, "#record")).to_contain_text('"payload_base64": "AP8="')
    role(page, "button", name="Get", exact=True).click()
    expect(control(page, "#record")).to_contain_text("<img src=x onerror=alert(1)>")
    expect(control(page, "#record img")).to_have_count(0)
    control(page, "#query").fill("[1,0,0]")
    control(page, "#filter").fill(json.dumps([{"op": "eq", "field": "category", "value": metadata["category"]}]))
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    expect(control(page, "#hits tbody")).to_contain_text("18446744073709551615")
    expect(control(page, "#inspector")).to_contain_text('"distance": 0')
    for metric, distance in [("cosine", 0), ("inner_product", -1)]:
        control(page, "#metric").select_option(metric)
        role(page, "button", name="Search", exact=True).click()
        expect(control(page, "#inspector")).to_contain_text(f'"metric": "{metric}"')
        expect(control(page, "#inspector")).to_contain_text(f'"distance": {distance}')
        expect(control(page, "#hits caption")).to_contain_text(metric)
    control(page, "#metric").select_option("squared_l2")
    control(page, "#mode").select_option("approximate_ready")
    expect(control(page, "#metric")).to_be_enabled()
    expect(control(page, '#metric option[value="inner_product"]')).to_have_js_property("disabled", True)
    expect(control(page, "#filter")).to_be_enabled()
    expect(control(page, "#ef")).to_be_enabled()
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#status")).to_contain_text("index_not_ready")
    # A failed refresh is not an empty successful result: retain the last
    # inner-product result, visibly labelled, without an exact-search fallback.
    expect(control(page, "#search-summary")).to_contain_text("Refresh failed; previous results retained.")
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    expect(control(page, "#hits tbody")).to_contain_text("18446744073709551615")
    expect(control(page, "#hits caption")).to_contain_text("inner_product")
    expect(control(page, "#inspector")).to_contain_text("index_not_ready")
    role(page, "button", name="Build HNSW", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#build-result")).to_contain_text("Build completed")
    expect(control(page, "#index-status")).to_contain_text("Ready;")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    control(page, "#metric").select_option("cosine")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#status")).to_contain_text("index_not_ready")
    role(page, "button", name="Build HNSW", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#index-status")).to_contain_text("metric cosine")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    expect(control(page, "#hits caption")).to_contain_text("cosine")
    control(page, "#metric").select_option("squared_l2")
    control(page, "#mode").select_option("approximate")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#inspector")).to_contain_text('"mode": "approximate"')
    # Client-side validation must not issue a malformed request.
    control(page, "#id").fill("01")
    role(page, "button", name="Get", exact=True).click()
    expect(control(page, "#status")).to_contain_text("canonical decimal u64")
    control(page, "#id").fill("18446744073709551615")
    role(page, "button", name="Delete", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#record")).to_contain_text('"deleted": true')
    expect(control(page, "#inspector")).not_to_contain_text(token)
    # Leave a distinct durable record for the restart check.
    control(page, "#id").fill("2")
    role(page, "button", name="Put / replace").click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#record")).to_contain_text('"committed": true')
    # Two pages, numerically ordered beyond JavaScript's safe-integer range.
    control(page, "#id").fill("18446744073709551615")
    role(page, "button", name="Put / replace").click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#record")).to_contain_text('"committed": true')
    control(page, "#page-size").fill("1")
    role(page, "button", name="First page / refresh", exact=True).click()
    expect(control(page, "#vectors tbody")).to_contain_text("2")
    expect(control(page, "#vectors tbody")).not_to_contain_text("18446744073709551615")
    expect(control(page, "#next-page")).to_be_enabled()
    control(page, "#next-page").click()
    expect(control(page, "#vectors tbody")).to_contain_text("18446744073709551615")
    expect(control(page, "#next-page")).to_be_disabled()
    role(page, "button", name="First page / refresh", exact=True).click()
    expect(control(page, "#next-page")).to_be_enabled()
    control(page, "#id").fill("2")
    role(page, "button", name="Put / replace").click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#vectors tbody tr")).to_have_count(0)
    expect(control(page, "#next-page")).to_be_disabled()
    expect(control(page, "#index-status")).to_contain_text("Data changed")
    role(page, "button", name="Refresh index status", exact=True).click()
    expect(control(page, "#index-status")).to_contain_text("Ready; metric squared_l2; buffer 2/256")
    assert_no_stored_credentials(page, token)
    role(page, "button", name="Disconnect", exact=True).click()
    expect(control(page, "#overview")).to_have_text("")
    expect(control(page, "#inspector")).to_have_text("")
    expect(control(page, "#metadata")).to_have_value("{}")
    expect(control(page, "#vectors tbody tr")).to_have_count(0)
    expect(control(page, "#index-status")).to_contain_text("not checked")
    expect(control(page, "#connection-badge")).to_have_text("Disconnected")
    expect(control(page, "#stat-dimensions")).to_have_text("--")
    page.reload()
    expect(control(page, "#status")).to_have_text("Disconnected")
    role(page, "button", name="Get", exact=True).click()
    expect(control(page, "#status")).to_contain_text("Connect before")
    assert not errors, errors
    # Responsive smoke check.
    page.set_viewport_size({"width": 390, "height": 844})
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
    page.set_viewport_size({"width": 1280, "height": 844})

def exercise_separate_app(page, base, token, expected_index="Ready;"):
    errors = []
    api_requests = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    page.on("request", lambda request: api_requests.append(request.url) if "/api/" in request.url else None)
    page.goto(base + "/")
    assert page.request.get(base + "/observability.js").status == 200
    expect(role(page, "heading", name="Connection", exact=True)).to_be_visible()
    expect(control(page, "#status")).to_have_text("Disconnected")
    control(page, "#token").fill("x" * 32)
    role(page, "button", name="Connect / refresh").click()
    expect(control(page, "#status")).to_contain_text("HTTP 401")
    control(page, "#token").fill(token)
    role(page, "button", name="Connect / refresh").click()
    expect(control(page, "#status")).to_have_text("Connected; overview refreshed")
    expect(control(page, "#overview")).to_contain_text('"dimensions": 3')
    expect(control(page, "#stat-records")).to_have_text("2")
    expect(control(page, "#stat-dimensions")).to_have_text("3")
    exercise_observability(page, 2)
    role(page, "navigation", name="Dashboard navigation").get_by_role("link", name="Search", exact=True).click()
    assert page.url.endswith("#search")
    expect(control(page, "#index-status")).to_contain_text(expected_index)
    role(page, "button", name="First page / refresh", exact=True).click()
    expect(control(page, "#vectors tbody tr")).to_have_count(2)
    control(page, "#query").fill("[1,0,0]")
    control(page, "#k").fill("2")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(2)
    expect(control(page, "#inspector")).to_contain_text('"distance": 0')
    expect(control(page, "#search-summary")).to_contain_text("2 neighbors returned")
    control(page, "#hits tbody button").first.click()
    expect(control(page, "#record")).to_contain_text('"payload_base64": "AP8="')
    expect(control(page, "#id")).to_have_value("2")
    assert any(url.endswith("/api/v1/database") for url in api_requests), api_requests
    # Newer servers route default-collection search through /v1/collections/default so its stored settings apply.
    assert any(url.endswith(("/api/v1/search", "/api/v1/collections/default/search")) for url in api_requests), api_requests
    assert_no_stored_credentials(page, token)
    role(page, "button", name="Disconnect", exact=True).click()
    expect(control(page, "#overview")).to_have_text("")
    assert not errors, errors


def exercise_console_tools(page, base, token):
    page.goto(base + "/")
    control(page, "#token").fill(token)
    role(page, "button", name="Connect / refresh").click()
    expect(control(page, "#status")).to_have_text("Connected; overview refreshed")
    expect(control(page, "#compatibility")).to_contain_text("version")
    expect(control(page, "#remember-preferences")).not_to_be_checked()
    control(page, "#tour-start").click()
    control(page, "#tour-dismiss").click()
    assert page.evaluate("localStorage.getItem('vlite-studio-tour')") == "dismissed"
    control(page, "#theme").select_option("dark")
    control(page, "#density").select_option("compact")
    assert page.evaluate("document.documentElement.dataset.theme") == "dark"
    assert_no_stored_credentials(page, token)
    control(page, "#remember-preferences").check()
    assert page.evaluate("JSON.parse(localStorage.getItem('vlite-appearance')).theme") == "dark"
    control(page, "#remember-preferences").uncheck()
    assert_no_stored_credentials(page, token)
    role(page, "button", name="Prepare 3 demo vectors", exact=True).click()
    expect(control(page, "#import-result")).to_contain_text("Preview only")
    role(page, "button", name="Commit previewed records", exact=True).click()
    expect(control(page, "#confirmation")).to_be_visible()
    role(page, "button", name="Cancel", exact=True).click()
    expect(control(page, "#stat-records")).to_have_text("2")
    control(page, "#import-file").set_input_files({
        "name": "invalid.json", "mimeType": "application/json",
        "buffer": b'[{"id":"01","vector":[1,0,0]}]',
    })
    role(page, "button", name="Validate / preview import", exact=True).click()
    expect(control(page, "#status")).to_contain_text("canonical decimal")
    records = [
        {"id": "101", "vector": [1, 0, 0], "metadata": {"category": {"type": "string", "value": "demo"}}},
        {"id": "102", "vector": [0, 1, 0], "metadata": {"category": {"type": "integer", "value": "42"}}},
    ]
    control(page, "#import-file").set_input_files({
        "name": "records.json", "mimeType": "application/json",
        "buffer": json.dumps(records).encode(),
    })
    role(page, "button", name="Validate / preview import", exact=True).click()
    expect(control(page, "#import-result")).to_contain_text('"validated_records": 2')
    role(page, "button", name="Commit previewed records", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#import-result")).to_contain_text("2/2 operations committed")
    role(page, "button", name="First page / refresh", exact=True).click()
    expect(control(page, "#vectors tbody tr")).to_have_count(4)
    for label in ["Export current page JSON", "Export current page CSV"]:
        with page.expect_download() as event:
            role(page, "button", name=label, exact=True).click()
        assert event.value.suggested_filename.startswith("vlite-page.")
    control(page, "#vectors").get_by_role("button", name="101", exact=True).click()
    expect(control(page, "#id")).to_have_value("101")
    role(page, "tab", name="Vector", exact=True).click()
    expect(control(page, "#record-inspector-vector")).to_contain_text("3 components")
    expect(control(page, "#record-inspector-vector")).to_contain_text("[1,0,0]")
    role(page, "tab", name="Raw JSON", exact=True).click()
    expect(control(page, "#record-inspector-raw-content")).to_contain_text('"id": "101"')
    expect(control(page, "#record-inspector-raw-content")).to_contain_text('"category"')
    control(page, "#browse-columns").select_option(["category"])
    expect(control(page, "#vectors thead th")).to_have_count(3)
    control(page, "#browse-direction").select_option("descending")
    expect(control(page, "#vectors tbody tr").first.locator("button")).to_have_text("18446744073709551615")
    control(page, "#browse-sort").select_option("category")
    expect(control(page, "#vectors tbody tr").first.locator("button")).to_have_text("101")
    role(page, "button", name="Load JSON into fields", exact=True).click()
    field = control(page, "#metadata-fields .metadata-row").first
    field.get_by_label("Metadata field type", exact=True).select_option("integer")
    field.get_by_label("Metadata field value", exact=True).fill("9223372036854775807")
    role(page, "button", name="Apply fields to JSON draft", exact=True).click()
    assert json.loads(control(page, "#metadata").input_value())["category"]["value"] == "9223372036854775807"
    role(page, "button", name="Add metadata field", exact=True).click()
    last = control(page, "#metadata-fields .metadata-row").last
    last.get_by_label("Metadata field name", exact=True).fill("category")
    role(page, "button", name="Apply fields to JSON draft", exact=True).click()
    expect(control(page, "#status")).to_contain_text("Duplicate metadata field")
    last.get_by_role("button", name="Remove field", exact=True).click()
    role(page, "button", name="Get", exact=True).click()
    expect(control(page, "#status")).to_contain_text("HTTP 200")
    assert json.loads(control(page, "#metadata").input_value())["category"]["value"] == "demo"
    expect(control(page, "#payload-preview")).to_contain_text("empty payload")
    role(page, "button", name="Inspect 32 components", exact=True).click()
    expect(control(page, "#vector-slice-result")).to_contain_text('"end_exclusive": 3')
    role(page, "button", name="Find similar to saved ID", exact=True).click()
    expect(control(page, "#search-summary")).to_contain_text("neighbors returned")
    expect(role(page, "heading", name="Build a metadata filter", exact=True)).to_be_visible()
    control(page, "#filter-field").fill("category")
    control(page, "#filter-value").fill("demo")
    role(page, "button", name="Add AND condition", exact=True).click()
    assert json.loads(control(page, "#filter").input_value())[0]["value"]["value"] == "demo"
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    control(page, "#query-name").fill("demo-filter")
    with page.expect_download() as query_event:
        role(page, "button", name="Download query", exact=True).click()
    with open(query_event.value.path(), encoding="utf-8") as stream:
        saved = json.load(stream)
    assert saved["name"] == "demo-filter" and token not in json.dumps(saved)
    control(page, "#query-file").set_input_files({"name": "query.json", "mimeType": "application/json", "buffer": json.dumps(saved).encode()})
    control(page, "#query").fill("[0,0,1]")
    role(page, "button", name="Load query into form", exact=True).click()
    expect(control(page, "#query-workspace-status")).to_contain_text("form only")
    expect(control(page, "#query")).to_have_value("[1,0,0]")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    control(page, "#query-file").set_input_files({
        "name": "unsafe-query.json", "mimeType": "application/json",
        "buffer": json.dumps({**saved, "token": "not-allowed"}).encode(),
    })
    role(page, "button", name="Load query into form", exact=True).click()
    expect(control(page, "#status")).to_contain_text("without extra fields")
    expect(control(page, "#query")).to_have_value("[1,0,0]")
    role(page, "button", name="Clear filter", exact=True).click()
    control(page, "#compare-id").fill("102")
    role(page, "button", name="Compare with editor ID", exact=True).click()
    expect(control(page, "#record-comparison")).to_contain_text('"squared_l2": 2')
    role(page, "button", name="Analyze first 32 records", exact=True).click()
    expect(control(page, "#sample-analysis")).to_contain_text('"sampled_records": 4')
    expect(control(page, "#sample-analysis")).to_contain_text('"mixed_types": true')
    expect(control(page, "#projection svg circle")).to_have_count(4)
    role(page, "button", name="Build HNSW", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#build-result")).to_contain_text("Build completed")
    role(page, "button", name="Compare exact / ready ANN", exact=True).click()
    expect(control(page, "#search-comparison")).to_contain_text('"overlap_fraction": 1')
    control(page, "#mode").select_option("approximate_ready")
    control(page, "#filter-field").fill("category")
    control(page, "#filter-type").select_option("integer")
    control(page, "#filter-value").fill("42")
    role(page, "button", name="Add AND condition", exact=True).click()
    expect(control(page, "#mode")).to_have_value("approximate_ready")
    control(page, "#k").fill("1")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    expect(control(page, "#hits tbody button")).to_have_text("102")
    expect(control(page, "#search-summary")).to_contain_text("no exact fallback")
    role(page, "button", name="Compare exact / ready ANN", exact=True).click()
    expect(control(page, "#search-comparison")).to_contain_text('"exact_count": 1')
    expect(control(page, "#search-comparison")).to_contain_text('"ann_count": 1')
    control(page, "#filter").fill('[{"op":"eq","field":"category","value":{"type":"integer","value":"999"}}]')
    role(page, "button", name="Compare exact / ready ANN", exact=True).click()
    expect(control(page, "#search-comparison")).to_contain_text('"overlap_fraction": null')
    expect(control(page, "#search-comparison")).to_contain_text('"exact_count": 0')
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(0)
    role(page, "button", name="Clear filter", exact=True).click()
    control(page, "#mode").select_option("exact")
    control(page, "#k").fill("10")
    control(page, "#scenario").fill(json.dumps({
        "name": "invalid-credential-field",
        "steps": [{"path": "/v1/vectors/101", "expected_ids": ["101"], "headers": {"Authorization": "not-allowed"}}],
    }))
    role(page, "button", name="Run scenario", exact=True).click()
    expect(control(page, "#status")).to_contain_text("credentials/headers are not allowed")
    control(page, "#scenario").fill(json.dumps({
        "name": "get-demo",
        "steps": [{"path": "/v1/vectors/101", "expected_ids": ["101"]}],
    }))
    role(page, "button", name="Run scenario", exact=True).click()
    expect(control(page, "#scenario-result")).to_contain_text('"passed": true')
    with page.expect_download():
        role(page, "button", name="Download scenario", exact=True).click()
    role(page, "button", name="Generate redacted examples", exact=True).click()
    expect(control(page, "#code-examples")).to_contain_text("VLITE_TOKEN")
    expect(control(page, "#code-examples")).not_to_contain_text(token)
    role(page, "button", name="Send request", exact=True).click()
    expect(control(page, "#inspector")).to_contain_text('"records": 4')
    role(page, "button", name="Refresh health snapshot", exact=True).click()
    expect(control(page, "#health-result")).to_contain_text('"observed_at"')
    role(page, "button", name="First page / refresh", exact=True).click()
    expect(control(page, "#vectors tbody tr")).to_have_count(4)
    control(page, "#batch-ids").fill("101,102")
    control(page, "#batch-metadata").fill('{"tag":{"type":"string","value":"updated"}}')
    role(page, "button", name="Replace selected metadata", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#import-result")).to_contain_text("2/2 operations committed")
    role(page, "button", name="First page / refresh", exact=True).click()
    expect(control(page, "#vectors tbody")).to_contain_text("updated")
    role(page, "button", name="Delete selected", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#import-result")).to_contain_text("2/2 operations committed")
    held = []
    page.route("**/api/v1/vectors/999", lambda route: held.append(route))
    control(page, "#id").fill("999")
    role(page, "button", name="Get", exact=True).click()
    expect(control(page, "#cancel-request")).to_be_enabled()
    role(page, "button", name="Stop waiting", exact=True).click()
    expect(control(page, "#status")).to_contain_text("browser stopped waiting")
    expect(control(page, "#status")).to_contain_text("server work was not cancelled")
    expect(control(page, "#status")).to_contain_text("Read not completed; previous results may still be displayed.")
    expect(control(page, "#error-help")).to_contain_text("never automatically retried")
    assert len(held) == 1
    held[0].abort()
    page.unroute("**/api/v1/vectors/999")
    role(page, "button", name="Connect / refresh", exact=True).click()
    expect(control(page, "#status")).to_have_text("Connected; overview refreshed")
    role(page, "button", name="First page / refresh", exact=True).click()
    expect(control(page, "#vectors tbody tr")).to_have_count(2)
    page.route("**/api/v1/vectors/999", lambda route: route.fulfill(
        status=500, content_type="application/json",
        body='{"error":{"code":"internal","message":"Write outcome unknown"}}'))
    control(page, "#id").fill("999")
    role(page, "button", name="Put / replace", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#status")).to_contain_text("HTTP 500")
    expect(control(page, "#stat-records")).to_have_text("--")
    expect(control(page, "#vectors tbody tr")).to_have_count(0)
    page.unroute("**/api/v1/vectors/999")
    role(page, "button", name="Disconnect", exact=True).click()
    expect(control(page, "#sample-analysis")).to_have_text("")
    expect(control(page, "#code-examples")).to_have_text("")
    page.set_viewport_size({"width": 390, "height": 844})
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
    page.set_viewport_size({"width": 1280, "height": 844})


def exercise_operations(page, base, token, read_token):
    page.goto(base + "/")
    control(page, "#token").fill(token)
    role(page, "button", name="Connect / refresh").click()
    expect(control(page, "#status")).to_have_text("Connected; overview refreshed")
    role(page, "button", name="Inspect credential sources", exact=True).click()
    expect(control(page, "#token-result")).to_contain_text('"read_only_configured": true')
    role(page, "button", name="Read sanitized audit trail", exact=True).click()
    expect(control(page, "#audit-result")).to_contain_text('"capacity": 256')
    expect(control(page, "#audit-result")).not_to_contain_text(token)
    expect(role(page, "heading", name="HNSW configuration and background maintenance", exact=True)).to_be_visible()
    role(page, "button", name="Read maintenance policy", exact=True).click()
    expect(control(page, "#maintenance-result")).to_contain_text('"pressure"')
    control(page, "#hnsw-m").fill("8")
    control(page, "#hnsw-ef").fill("40")
    role(page, "button", name="Start background rebuild", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#rebuild-result")).to_contain_text('"outcome": "running"')
    deadline = time.monotonic() + 600
    while True:
        with page.expect_response(lambda response: response.url.endswith("/index/hnsw/rebuild/poll")) as polled:
            role(page, "button", name="Poll / publish rebuild", exact=True).click()
        assert polled.value.status == 200
        outcome = polled.value.json()["outcome"]
        if outcome != "running":
            assert outcome == "published", outcome
            break
        assert time.monotonic() < deadline, "rebuild did not finish"
        time.sleep(.05)
    expect(control(page, "#rebuild-result")).to_contain_text('"published"')
    control(page, "#query").fill("[1,0,0]")
    control(page, "#filter").fill("[]")
    control(page, "#mode").select_option("approximate_ready")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(2)
    role(page, "button", name="Replace maintenance policy", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#maintenance-result")).to_contain_text('"policy"')
    role(page, "button", name="Create consistent backup", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#backup-result")).to_contain_text('"inspection"')
    expect(control(page, "#backup-name option")).to_have_count(1)
    role(page, "button", name="Validate / stage restored copy", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#restore-name option")).to_have_count(1)
    role(page, "button", name="Select restored copy for restart", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#backup-result")).to_contain_text('"restart_required": true')
    role(page, "button", name="Disconnect", exact=True).click()
    control(page, "#token").fill(read_token)
    role(page, "button", name="Connect / refresh").click()
    expect(control(page, "#status")).to_have_text("Connected; overview refreshed")
    expect(control(page, "#compatibility")).to_contain_text('"role": "read_only"')
    expect(control(page, "#delete-vector")).to_be_disabled()
    expect(control(page, "#import-commit")).to_be_disabled()
    expect(control(page, "#create-backup")).to_be_disabled()
    control(page, "#mode").select_option("exact")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(2)
    # Direct API access still enforces the role independently of the console.
    headers = {"Authorization": "Bearer " + read_token}
    control(page, "#mode").select_option("approximate_ready")
    control(page, "#filter").fill('[{"op":"eq","field":"category","value":{"type":"string","value":"missing"}}]')
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(0)
    expect(control(page, "#status")).to_contain_text("HTTP 200")
    assert page.request.put(base + "/api/v1/vectors/500", headers=headers, data={"vector": [1, 0, 0]}).status == 403
    assert page.request.get(base + "/api/v1/admin/audit", headers=headers).status == 403
    assert page.request.post(base + "/api/v1/search", headers=headers, data={"vector": [1, 0, 0], "k": 1, "mode": "approximate"}).status == 403
    role(page, "button", name="Disconnect", exact=True).click()
    control(page, "#token").fill(token)
    role(page, "button", name="Connect / refresh").click()
    expect(control(page, "#status")).to_have_text("Connected; overview refreshed")
    role(page, "button", name="Revoke read-only credential", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#token-result")).to_contain_text('"read_only_configured": false')
    assert page.request.get(base + "/api/v1/database", headers=headers).status == 401
    role(page, "button", name="Reload configured token files", exact=True).click()
    role(page, "button", name="Confirm", exact=True).click()
    expect(control(page, "#status")).to_contain_text("Configured credentials reloaded")
    expect(control(page, "#connection-badge")).to_have_text("Disconnected")
    assert page.request.get(base + "/api/v1/database", headers=headers).status == 200


VLK_TOKEN = re.compile(r"vlk_[A-Za-z0-9]+_[A-Za-z0-9_-]{20,}")


def exercise_epic015_real(page, base, token):
    """EPIC-015 surfaces against the real API (no mocks); gated on what the server advertises."""
    headers = {"Authorization": "Bearer " + token}
    caps = page.request.get(base + "/api/v1/capabilities", headers=headers).json()
    features = caps.get("features", {})
    page.goto(base + "/")
    errors = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    control(page, "#token").fill(token)
    role(page, "button", name="Connect / refresh").click()
    expect(control(page, "#status")).to_have_text("Connected; overview refreshed")
    if not features.get("collections"):
        # Format v3 file: no invented surfaces; creation stays disabled and nothing is upgraded.
        expect(control(page, "#collection-create")).to_be_disabled()
        expect(control(page, "#job-start")).to_be_disabled()
        print(f"NOTE real EPIC-015: format_version={caps.get('format_version')}; collection/import surfaces correctly gated off")
        assert not errors, errors
        return
    before = page.request.get(base + "/api/v1/database", headers=headers).json()
    # Create an empty collection with explicit settings.
    control(page, "#new-collection-name").fill("e2e_docs")
    control(page, "#new-collection-dimensions").fill("2")
    control(page, "#new-collection-settings").fill('{"metric":"cosine","text_fields":["title"]}')
    control(page, "#collection-review").click()
    expect(control(page, "#collection-create-summary")).to_contain_text("nothing written")
    control(page, "#collection-create").click()
    confirm(page, "Create empty collection e2e_docs with 2 dimensions")
    expect(control(page, "#collections-table tbody")).to_contain_text("e2e_docs")
    # Switch (pending work forces the confirmation); fields follow the new dimensions.
    control(page, "#filter").fill('[{"op":"eq","field":"category","value":{"type":"string","value":"x"}}]')
    control(page, "#collection").select_option("e2e_docs")
    confirm(page, "Switch from collection default to e2e_docs")
    expect(control(page, "#collection-badge")).to_have_text("Collection: e2e_docs")
    expect(control(page, "#query")).to_have_value("[1,0]")
    expect(control(page, "#metric")).to_have_value("cosine")
    # Record write goes to the selected collection only; revision/ETag shown on Get.
    control(page, "#id").fill("1")
    control(page, "#record-vector").fill("[1,0]")
    control(page, "#metadata").fill('{"title":{"type":"string","value":"vector database primer"}}')
    role(page, "button", name="Put / replace").click()
    confirm(page, "collection e2e_docs")
    expect(control(page, "#status")).to_contain_text("HTTP 204")
    role(page, "button", name="Get", exact=True).click()
    expect(control(page, "#record-revision")).to_contain_text("revision")
    assert page.request.get(base + "/api/v1/collections/e2e_docs/vectors/1", headers=headers).status == 200
    # Import job: 3 valid rows in 2 chunks, then finish (committed chunks retained).
    rows = "\n".join(json.dumps({"id": str(10 + index), "vector": [1, index + 1], "metadata": {"title": {"type": "string", "value": f"doc {index}"}}}) for index in range(3))
    control(page, "#job-file").set_input_files(files=[{"name": "rows.ndjson", "mimeType": "application/x-ndjson", "buffer": rows.encode()}])
    control(page, "#job-chunk-size").fill("2")
    control(page, "#job-prepare").click()
    expect(control(page, "#job-result")).to_contain_text("2")
    control(page, "#job-start").click()
    confirm(page, "into collection e2e_docs")
    expect(control(page, "#job-progress")).to_have_attribute("value", "2")
    control(page, "#job-finish").click()
    confirm(page, "stay")
    expect(control(page, "#job-result")).to_contain_text("finished")
    info = page.request.get(base + "/api/v1/collections/e2e_docs", headers=headers).json()
    assert info["live_records"] == 4, info
    # Search modes on the collection route: hybrid (score, higher is better) and auto with explain.
    if features.get("search_hybrid"):
        control(page, "#filter").fill("[]")
        control(page, "#mode").select_option("hybrid")
        control(page, "#search-text").fill("vector database")
        role(page, "button", name="Search", exact=True).click()
        expect(control(page, "#hits caption")).to_contain_text("hybrid score, higher is better")
        expect(control(page, "#hits tbody tr").first).to_contain_text("1")
    control(page, "#mode").select_option("auto")
    control(page, "#explain").check()
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#search-plan")).to_contain_text("exact_scan")
    expect(control(page, "#hits caption")).to_contain_text("lower distance is better")
    # Maintenance read uses the selected collection's route; the legacy (global) save is disabled here.
    role(page, "button", name="Read maintenance policy", exact=True).click()
    expect(control(page, "#maintenance-result")).to_contain_text("e2e_docs")
    expect(control(page, "#save-maintenance")).to_be_disabled()
    # Scoped credential: one-time masked secret; never in page text, inputs, storage, URL or inspector.
    if features.get("scoped_credentials"):
        expect(control(page, "#cred-collections")).to_have_value("e2e_docs")
        control(page, "#cred-name").fill("e2e-reader")
        control(page, "#cred-create").click()
        confirm(page, "collections [e2e_docs]")
        expect(control(page, "#cred-secret")).to_contain_text("Shown once")
        assert not VLK_TOKEN.search(visible_state(page)), "one-time secret leaked into page state"
        listed = page.request.get(base + "/api/v1/admin/credentials", headers=headers).json()
        created = next(item for item in listed["items"] if item["name"] == "e2e-reader")
        control(page, "#cred-clear").click()
        expect(control(page, "#cred-secret")).to_have_text("No new secret.")
        role(page, "button", name="Revoke " + created["id"]).click()
        confirm(page, "Revoke credential " + created["id"])
        expect(control(page, "#cred-table tbody")).to_contain_text("revoked")
        assert not VLK_TOKEN.search(visible_state(page))
    # Schedulers are off by default; diagnostics are bounded.
    if features.get("backup_schedule"):
        control(page, "#backup-schedule-read").click()
        expect(control(page, "#backup-schedule")).to_have_value(re.compile(r'"enabled": false'))
    if features.get("auto_maintenance"):
        control(page, "#auto-maintenance-read").click()
        expect(control(page, "#auto-maintenance")).to_have_value(re.compile(r'"enabled": false'))
    if features.get("backup_verify_drill"):
        # Fresh v4 server: create one consistent backup, then verify it.
        control(page, "#create-backup").click()
        role(page, "button", name="Confirm", exact=True).click()
        expect(control(page, "#backup-result")).to_contain_text('"manifest"')
        control(page, "#list-backups").click()
        expect(control(page, "#backup-name option")).not_to_have_count(0)
        control(page, "#backup-verify").click()
        confirm(page, "Verify backup-")
        expect(control(page, "#backup-result")).to_contain_text('"verified": true')
    if features.get("slow_query_log"):
        control(page, "#slow-queries").click()
        expect(control(page, "#slow-summary")).to_contain_text("threshold")
    if features.get("prometheus_metrics"):
        control(page, "#prometheus").click()
        expect(control(page, "#prometheus-result")).to_contain_text("vlite_")
    # The default collection was never written by the e2e_docs work.
    after = page.request.get(base + "/api/v1/database", headers=headers).json()
    assert after.get("records") == before.get("records"), (before, after)
    role(page, "button", name="Disconnect", exact=True).click()
    assert not VLK_TOKEN.search(visible_state(page))
    assert not errors, errors
    print(f"PASS EPIC-015 real API: format_version={caps.get('format_version')}; collection create/switch/isolated write, import job, hybrid/auto, collection-route maintenance, one-time credential create/revoke, schedulers, verify, diagnostics")


# ---------------------------------------------------------------------------
# EPIC-015 console surfaces against a route-mocked API that follows the Wave 2
# server contract. Runs without Docker (``python tests/dashboard_e2e.py --mocked``)
# and is also part of the full Docker run. Static assets are the real files under
# src/dashboard, served with a strict no-inline CSP.
# ---------------------------------------------------------------------------
MOCK_ORIGIN = "http://vlite.test"
DASHBOARD_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "src", "dashboard")
CSP = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; form-action 'none'; base-uri 'none'; frame-ancestors 'none'"
ALL_FEATURES = ["background_rebuild", "maintenance", "backup_restore", "audit", "filtered_ann", "collections", "imports", "per_record_etags",
                "search_auto", "search_hybrid", "search_explain", "quantization", "auto_maintenance", "backup_schedule", "backup_verify_drill",
                "slow_query_log", "prometheus_metrics", "scoped_credentials", "persistent_audit"]
SECRET_TAIL = "Q" * 43


def settings(metric="squared_l2", text_fields=()):
    return {"metric": metric, "hnsw": {"m": 16, "ef_construction": 200, "seed": "0"}, "quantization": "none", "rerank_multiplier": 4,
            "planner": {"exact_max_candidates": 4096, "exact_max_selectivity_ppm": 20000}, "metadata_indexes": [], "text_fields": list(text_fields)}


class FakeApi:
    """Minimal in-memory Wave 2 API: records every request so tests can assert routing and headers."""

    def __init__(self, role="admin", features=ALL_FEATURES, format_version=4, scope=None):
        self.caps = {"api_version": 1, "role": role, "features": {name: True for name in features}}
        if format_version is not None:
            self.caps["format_version"] = format_version
        if scope:
            self.caps["scope"] = scope
        self.collections = {"default": {"id": 0, "name": "default", "dimensions": 3, "live_records": 1, "settings": settings(), "settings_persisted": True},
                            "docs": {"id": 1, "name": "docs", "dimensions": 2, "live_records": 1, "settings": settings("cosine", ["title"]), "settings_persisted": True}}
        if scope and "*" not in scope["collections"]:
            self.collections = {name: value for name, value in self.collections.items() if name in scope["collections"]}
        self.records = {("default", "1"): {"vector": [1, 0, 0], "revision": 1}, ("docs", "5"): {"vector": [1, 0], "revision": 1}}
        self.log = []
        self.jobs = {}
        self.reject_chunk_once = {2}
        self.credentials = []
        self.issued = []
        self.schedule = {"enabled": False, "interval_secs": 86400, "retention_enabled": False, "retain_last": 7, "retain_days": 0, "drill_every": 1, "drill_max_bytes": 4294967296}
        self.auto = {"enabled": False, "interval_ms": 60000, "stale_ratio": 0.2, "rebuild_on_low_recall": False, "min_recall": 0.9, "quality_samples": 8, "limits": {}}

    def etag(self, collection, record_id):
        return f'"r2-{"a" * 16}-{self.collections[collection]["id"]}-{record_id}-{self.records[(collection, record_id)]["revision"]}-{"b" * 64}"'

    def handle(self, route):
        request = route.request
        url = request.url[len(MOCK_ORIGIN):]
        path, _, query = url.partition("?")
        if path == "/dashboard" or path.startswith("/dashboard/"):
            # Serve the real navigation.js alongside app.js and observability.js:
            # mocked API tests exercise production routing, never a show-all variant.
            name = "index.html" if path in ("/dashboard", "/dashboard/") else path[len("/dashboard/"):]
            kind = {"html": "text/html", "js": "text/javascript", "css": "text/css"}[name.rsplit(".", 1)[1]]
            with open(os.path.join(DASHBOARD_DIR, name), "rb") as stream:
                return route.fulfill(status=200, body=stream.read(), headers={"content-type": kind + "; charset=utf-8", "content-security-policy": CSP})
        body = json.loads(request.post_data) if request.post_data else None
        self.log.append({"method": request.method, "path": path, "query": query, "body": body, "headers": request.headers})
        status, value, headers = self.api(request.method, path, query, body, request.headers)
        headers = {"cache-control": "no-store", "x-request-id": str(len(self.log)), **headers}
        if status == 204:
            return route.fulfill(status=204, headers=headers)
        if isinstance(value, str):
            return route.fulfill(status=status, body=value, headers={**headers, "content-type": "text/plain; version=0.0.4"})
        return route.fulfill(status=status, body=json.dumps(value), headers={**headers, "content-type": "application/json"})

    @staticmethod
    def error(status, code):
        return status, {"error": {"code": code, "message": code.replace("_", " ")}}, {}

    def collection_route(self, path):
        parts = path.split("/")
        if path.startswith("/v1/collections/") and len(parts) >= 4:
            return parts[3], "/" + "/".join(parts[4:]) if len(parts) > 4 else ""
        return "default", path[3:]

    def api(self, method, path, query, body, headers):
        if not headers.get("authorization", "").startswith("Bearer "):
            return self.error(401, "unauthorized")
        if path == "/v1/capabilities":
            return 200, self.caps, {}
        if path == "/v1/collections" and method == "GET":
            return 200, {"items": list(self.collections.values()), "format_version": self.caps.get("format_version")}, {}
        if path == "/v1/database":
            return 200, {"dimensions": 3, "records": 1}, {}
        if path == "/metrics":
            return (200, "vlite_records 2\n", {}) if query == "format=prometheus" else (200, {"database_bytes": 4096}, {})
        if path == "/v1/admin/credentials" and method == "GET":
            return 200, {"items": self.credentials, "capacity": 256}, {}
        if path == "/v1/admin/credentials" and method == "POST":
            if getattr(self, "indeterminate", False):  # change applied, durability unconfirmed, no token returned
                self.issue(body, None)
                return self.error(503, "credentials_commit_indeterminate")
            return self.issue(body, None)
        if path.startswith("/v1/admin/credentials/") and path.endswith("/rotate"):
            previous = next(item for item in self.credentials if item["id"] == path.split("/")[4])
            return self.issue({**previous, "name": previous["name"]}, previous)
        if path == "/v1/admin/backups/schedule":
            if method == "PUT":
                self.schedule.update(body)
            return 200, {"config": self.schedule, "state": {"runs": 0, "failures": 0, "skipped_busy": 0, "last_success_unix_ms": None, "last_failure_unix_ms": None,
                                                           "last_error": None, "last_backup": None, "drills": 0, "drill_failures": 0, "last_drill_unix_ms": None,
                                                           "retention_deleted": 0, "retained": 0}}, {}
        if path == "/v1/admin/maintenance/auto":
            if method == "PUT":
                self.auto.update({key: value for key, value in body.items() if key != "interval_secs"})
                if "interval_secs" in body:
                    self.auto["interval_ms"] = body["interval_secs"] * 1000
            return 200, {"config": self.auto, "state": {"ticks": 0, "skipped_busy": 0, "rebuilds_started": 0, "errors": 0, "last_tick_unix_ms": None,
                                                       "last_collection": None, "last_action": None, "last_error": None}}, {}
        if path == "/v1/admin/diagnostics/slow-queries":
            return 200, {"threshold_ms": 250, "capacity": 256, "items": [{"request_id": "9", "unix_ms": 1700000000000, "route": "/v1/collections/{c}/search",
                         "collection": "docs", "mode": "auto", "plan_strategy": "exact_scan", "k": 10, "ef_search": None, "filter_fields": ["year"],
                         "duration_ms": 412, "stats": {}, "result_count": 10}]}, {}
        if path == "/v1/admin/maintenance/auto/tick" and method == "POST":
            return 200, {"outcome": "idle", "collection": None, "action": None}, {}
        if path == "/v1/admin/backups/schedule/run" and method == "POST":
            return 200, {"outcome": "completed", "backup": "vlite-backup-1.vlite"}, {}
        if path == "/v1/index/hnsw/maintenance" and method == "PUT":
            return 200, {"policy": body}, {}
        if path.startswith("/v1/imports/"):
            return self.imports(method, path.split("/")[3:], body)
        collection, rest = self.collection_route(path)
        if collection not in self.collections:
            return self.error(404, "collection_not_found")
        info = self.collections[collection]
        if rest == "" and method == "GET":
            return 200, {**info, "text_index": {"declared_fields": info["settings"]["text_fields"], "available": True, "documents": 1, "terms": 3, "postings": 3}}, {}
        if rest == "/index/hnsw" and method == "GET":
            built = getattr(self, "ready_index", {}).get(collection)
            if built:  # a published index whose explicit identity differs from the stored settings
                return 200, {"ready": True, "config": built["config"], "metric": built["metric"], "quantization": "none", "rerank_multiplier": 4,
                             "buffered_ids": 0, "buffer_vector_bytes": 0, "graph_nodes": 1, "stale_nodes": 0, "memory": None,
                             "max_buffer_ids": 256, "max_buffer_vector_bytes": 1048576, "settings_ready": False}, {}
            return 200, {"ready": False, "config": info["settings"]["hnsw"], "metric": None, "quantization": "none", "rerank_multiplier": 4, "buffered_ids": 0,
                         "buffer_vector_bytes": 0, "graph_nodes": 0, "stale_nodes": 0, "memory": None, "max_buffer_ids": 256,
                         "max_buffer_vector_bytes": 1048576, "settings_ready": True}, {}
        if rest == "/index/hnsw/maintenance" and method == "GET":
            policy = {"config": info["settings"]["hnsw"], "metric": info["settings"]["metric"], "rebuild_at_buffered_ids": 128}
            result = {"policy": policy, "status": {"pressure": False}}
            if path.startswith("/v1/collections/"):
                result["identity"] = {"collection": collection, "metric": info["settings"]["metric"], "quantization": info["settings"]["quantization"]}
            return 200, result, {}
        if rest == "/index/hnsw/maintenance/tick" and method == "POST":
            return 200, {"outcome": "idle", "started": False, "status": {"pressure": False}}, {}
        if rest.startswith("/vectors/"):
            record_id = rest.split("/")[2]
            key = (collection, record_id)
            if method == "GET":
                if key not in self.records:
                    return self.error(404, "not_found")
                record = self.records[key]
                return 200, {"id": record_id, "vector": record["vector"], "payload_base64": "", "metadata": {}, "revision": str(record["revision"])}, {"etag": self.etag(collection, record_id)}
            if method == "PUT":
                if "if-match" in headers and (key not in self.records or headers["if-match"] != self.etag(collection, record_id)):
                    return self.error(412, "precondition_failed")
                revision = self.records.get(key, {"revision": 0})["revision"] + 1
                self.records[key] = {"vector": body["vector"], "revision": revision}
                return 204, None, {"etag": self.etag(collection, record_id)}
        if rest == "/search" and method == "POST":
            if body["mode"] == "hybrid":
                hits = [{"id": "5", "score": 0.032, "distance": 0.0, "bm25": 1.7, "vector_rank": 1, "text_rank": 1},
                        {"id": "9", "score": 0.016, "distance": None, "bm25": 0.4, "vector_rank": None, "text_rank": 2}]
            else:
                hits = [{"id": "5", "distance": 0.0}]
            result = {"hits": hits}
            if body.get("explain"):
                result["stats"] = {"records_scanned": 1, "distance_evaluations": 1, "quantized_distance_evaluations": 0, "graph_nodes_visited": 0,
                                   "buffered_records_merged": 0, "reranked_candidates": 0, "index_candidates": 0}
                result["plan"] = {"strategy": "exact_scan", "estimated_candidates": 1, "live_records": 1, "index_field": None, "reason": "small collection"} if body["mode"] == "auto" else None
            return 200, result, {}
        if rest == "/imports" and method == "POST":
            job = str(len(self.jobs) + 7)
            self.jobs[job] = {"job": job, "collection": collection, "collection_dropped": False, "state": "open", "last_chunk": 0,
                              "rows_committed": 0, "rows_skipped": 0, "last_digest": None, "durable": False}
            return 201, self.jobs[job], {}
        return self.error(404, "mock_route_not_found")

    def issue(self, body, previous):
        number = len(self.issued) + 1
        credential = {"id": f"cred{number:08d}", "name": body["name"], "collections": body["collections"], "operations": body["operations"],
                      "lineage": "a" * 16, "created_unix_ms": 1, "expires_unix_ms": body["expires_unix_ms"], "revoked_unix_ms": None,
                      "rotated_from": previous["id"] if previous else None, "state": "active"}
        token = f"vlk_{credential['id']}_{SECRET_TAIL[:-1]}{number}"
        self.issued.append(token)
        self.credentials.append(credential)
        result = {"credential": credential, "token": token}
        if previous:
            result["previous"] = previous
        return 201, result, {}

    def imports(self, method, parts, body):
        job = self.jobs.get(parts[0])
        if job is None:
            return self.error(404, "import_not_found")
        if len(parts) == 1:
            return 200, job, {}
        if parts[1] in ("finish", "abort"):
            job["state"] = "finished" if parts[1] == "finish" else "aborted"
            return 200, job, {}
        chunk = int(parts[2])
        report = {"job": job["job"], "chunk": chunk, "rows_skipped": 0, "invalid_rows": 0, "invalid": [], "revision": "3", "status": job}
        if chunk <= job["last_chunk"]:
            return 200, {**report, "outcome": "already_committed", "rows_committed": 0}, {}
        if chunk != job["last_chunk"] + 1:
            return self.error(409, "import_conflict")
        if chunk in self.reject_chunk_once:
            self.reject_chunk_once.discard(chunk)
            return 422, {**report, "error": {"code": "import_rows_invalid", "message": "invalid rows"}, "outcome": "rejected", "rows_committed": 0,
                         "invalid_rows": 1, "invalid": [{"row": 0, "code": "invalid_input", "message": "bad vector"}]}, {}
        job.update(last_chunk=chunk, rows_committed=job["rows_committed"] + len(body["operations"]), durable=True, last_digest="c" * 64)
        return 200, {**report, "outcome": "committed", "rows_committed": len(body["operations"])}, {}


def mocked_page(browser, fake):
    context = browser.new_context(viewport={"width": 1280, "height": 900})
    context.route(MOCK_ORIGIN + "/**", fake.handle)
    page = context.new_page()
    page.set_default_timeout(30000)
    errors = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    page.on("console", lambda message: errors.append(message.text) if message.type == "error" and "Failed to load resource" not in message.text else None)
    page.goto(MOCK_ORIGIN + "/dashboard")
    control(page, "#token").fill("admin-" + "t" * 32)
    role(page, "button", name="Connect / refresh").click()
    expect(control(page, "#status")).to_have_text("Connected; overview refreshed")
    return context, page, errors


def confirm(page, expected, accept=True):
    expect(control(page, "#confirmation")).to_be_visible()
    expect(control(page, "#confirmation-text")).to_contain_text(expected)
    control(page, "#confirmation").get_by_role("button", name="Confirm" if accept else "Cancel").click()
    expect(control(page, "#confirmation")).to_be_hidden()


def option_disabled(page, value):
    # Playwright's is_disabled() does not report <option disabled>; read the DOM property.
    return control(page, f'#mode option[value="{value}"]').evaluate("option => option.disabled")


def visible_state(page):
    """Everything a user, a screenshot or an extension could read from the page and browser storage."""
    return page.evaluate("""() => JSON.stringify({
        text: document.body.innerText,
        values: [...document.querySelectorAll("input, textarea, select")].map((element) => element.value),
        local: Object.entries(localStorage), session: Object.entries(sessionStorage), title: document.title, url: location.href })""")


def exercise_navigation_mocked(browser):
    fake = FakeApi()
    context, page, errors = mocked_page(browser, fake)
    try:
        exercise_layout(page, MOCK_ORIGIN + "/dashboard")
        page.locator("#token").fill("admin-" + "t" * 32)
        page.get_by_role("button", name="Connect / refresh").click()
        expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
        # A missing locator still reaches the normal Playwright assertion, while
        # navigating to a secret-bearing section never unhides the secret field.
        expect(control(page, "#does-not-exist")).to_have_count(0)
        expect(control(page, "#cred-copy")).to_be_disabled()
        expect(page.locator("#cred-secret")).to_have_text("No new secret.")
        assert page.locator("#credentials input[type=password]").count() == 0
        expect(page.locator("main > section:visible")).to_have_count(1)
        assert not errors, errors
    finally:
        context.close()
    print("PASS focused navigation: desktop/mobile widths, drawer focus/Escape, deep links, history, keyboard tabs and preserved drafts")


def exercise_read_loading_mocked(browser):
    fake = FakeApi()
    context, page, errors = mocked_page(browser, fake)
    held = []
    try:
        control(page, "#query").fill("[1,0,0]")
        role(page, "button", name="Search", exact=True).click()
        expect(control(page, "#hits tbody tr")).to_have_count(1)
        previous = page.locator("#hits tbody").text_content()
        page.route("**/v1/collections/default/search", lambda route: held.append(route))
        search = page.get_by_role("button", name="Search", exact=True)
        search.click()
        expect(search).to_have_attribute("data-loading", "true")
        expect(search).to_have_attribute("aria-disabled", "true")
        expect(page.locator("#hits")).to_have_attribute("aria-busy", "true")
        expect(page.locator("#hits tbody")).to_have_text(previous)
        expect(page.locator("#hits")).to_be_visible()
        # Navigation remains usable while the read is pending; it does not
        # discard the draft, previous results, connection or active collection.
        nav = page.get_by_role("navigation", name="Dashboard navigation")
        nav.get_by_role("link", name="Overview", exact=True).click()
        expect(page.locator("#database")).to_be_visible()
        nav.get_by_role("link", name="Search", exact=True).click()
        expect(page.locator("#query")).to_have_value("[1,0,0]")
        expect(page.locator("#hits tbody")).to_have_text(previous)
        expect(page.locator("#connection-badge")).to_have_text("Connected")
        assert len(held) == 1
        held[0].fulfill(status=200, content_type="application/json", body='{"hits":[{"id":"7","distance":0.25}]}')
        expect(search).not_to_have_attribute("data-loading", "true")
        expect(search).not_to_have_attribute("aria-disabled", "true")
        expect(page.locator("#hits")).not_to_have_attribute("aria-busy", "true")
        expect(page.locator("#hits tbody")).to_contain_text("7")
        page.unroute("**/v1/collections/default/search")
        # Inspect a real mocked record through the UI, then exercise accessible
        # inspector tabs without helpers auto-activating any panel.
        control(page, "#id").fill("1")
        role(page, "button", name="Get", exact=True).click()
        expect(control(page, "#record")).to_contain_text('"id": "1"')
        nav.get_by_role("link", name="Collections", exact=True).click()
        inspector = page.get_by_role("tablist", name="Selected record views")
        payload = inspector.get_by_role("tab", name="Payload", exact=True)
        payload.click()
        expect(payload).to_have_attribute("aria-selected", "true")
        expect(page.locator("#record-inspector-payload")).to_be_visible()
        payload.press("ArrowRight")
        metadata = inspector.get_by_role("tab", name="Metadata", exact=True)
        expect(metadata).to_be_focused()
        expect(metadata).to_have_attribute("aria-selected", "true")
        expect(page.locator("#record-inspector-metadata")).to_be_visible()
        expect(page.locator("#record-inspector-payload")).to_be_hidden()
        metadata.press("End")
        expect(inspector.get_by_role("tab", name="Raw JSON", exact=True)).to_be_focused()
        expect(page.locator("#record-inspector-raw")).to_be_visible()
        page.keyboard.press("Home")
        expect(payload).to_be_focused()
        expect(page.locator("#record-inspector-raw")).to_be_hidden()
        assert not errors, errors
    finally:
        for route in held:
            try:
                route.abort()
            except PlaywrightError:
                pass
        context.close()
    print("PASS pending search preserves results/drafts and navigation; inspector tabs expose one accessible panel")


def exercise_collections_mocked(browser):
    fake = FakeApi()
    context, page, errors = mocked_page(browser, fake)
    expect(control(page, "#collection")).to_be_enabled()
    expect(control(page, "#collection option")).to_have_count(2)
    expect(control(page, "#collection-badge")).to_have_text("Collection: default")
    expect(control(page, "#collections-table tbody tr")).to_have_count(2)
    expect(control(page, "#collections-format")).to_contain_text("4")
    # Pending work (loaded record) requires confirmation; Cancel keeps the original collection.
    control(page, "#id").fill("1")
    role(page, "button", name="Get", exact=True).click()
    expect(control(page, "#record-revision")).to_contain_text("revision 1")
    control(page, "#collection").select_option("docs")
    confirm(page, "loaded record 1", accept=False)
    expect(control(page, "#collection-badge")).to_have_text("Collection: default")
    expect(control(page, "#collection")).to_have_value("default")
    control(page, "#collection").select_option("docs")
    confirm(page, "Switch from collection default to docs")
    expect(control(page, "#collection-badge")).to_have_text("Collection: docs")
    expect(control(page, "#status")).to_contain_text("Collection docs selected")
    expect(control(page, "#record")).to_have_text("")
    expect(control(page, "#index-status")).to_contain_text("collection docs")
    # Dimension change (3 -> 2): example fields follow the selected collection and stale-sized vectors are never sent.
    expect(control(page, "#record-vector")).to_have_value("[1,0]")
    expect(control(page, "#query")).to_have_value("[1,0]")
    sent = len(fake.log)
    control(page, "#query").fill("[1, 0, 0]")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#status")).to_contain_text("has 3 dimensions but collection docs uses 2")
    control(page, "#record-vector").fill("[1, 0, 0]")
    control(page, "#id").fill("9")
    role(page, "button", name="Put / replace").click()
    expect(control(page, "#status")).to_contain_text("has 3 dimensions but collection docs uses 2")
    assert len(fake.log) == sent, fake.log[sent:]
    control(page, "#query").fill("[1,0]")
    # Record routes go only to the selected collection; If-Match carries the loaded tag.
    control(page, "#id").fill("5")
    role(page, "button", name="Get", exact=True).click()
    expect(control(page, "#record-revision")).to_contain_text("Loaded from collection docs, revision 1")
    assert fake.log[-1]["path"] == "/v1/collections/docs/vectors/5", fake.log[-1]
    loaded_tag = fake.etag("docs", "5")
    fake.records[("docs", "5")]["revision"] += 1  # concurrent external write
    role(page, "button", name="Put / replace").click()
    confirm(page, "record 5 in collection docs")
    expect(control(page, "#record-revision")).to_contain_text("Conflict")
    put = fake.log[-1]
    assert put["method"] == "PUT" and put["path"] == "/v1/collections/docs/vectors/5" and put["headers"]["if-match"] == loaded_tag, put
    assert fake.records[("docs", "5")]["revision"] == 2  # nothing written
    assert not any(entry["method"] in ("PUT", "POST", "DELETE") and entry["path"].startswith("/v1/vectors") for entry in fake.log)
    # Hybrid search: score columns, explicit direction and collection route.
    control(page, "#mode").select_option("hybrid")
    expect(control(page, "#mode-help")).to_contain_text("HIGHER is better")
    control(page, "#query").fill("[1, 0]")
    control(page, "#search-text").fill("vector database")
    control(page, "#explain").check()
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits caption")).to_contain_text("hybrid score, higher is better")
    expect(control(page, "#hits thead th")).to_have_text(["ID", "Score (higher is better)", "Distance (lower is better)", "bm25", "vector_rank", "text_rank", "Record preview"])
    expect(control(page, "#hits tbody tr").nth(1)).to_contain_text("--")
    search = fake.log[-1]
    assert search["path"] == "/v1/collections/docs/search", search
    assert search["body"]["mode"] == "hybrid" and search["body"]["text"] == "vector database" and search["body"]["explain"] is True, search
    assert "quantization" not in search["body"] and "ef_search" not in search["body"], search
    control(page, "#mode").select_option("auto")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#search-plan")).to_contain_text("exact_scan")
    expect(control(page, "#hits caption")).to_contain_text("lower distance is better")
    # Resumable import: a rejected chunk stops the job; resume replays the last committed chunk
    # (already_committed) and continues from the durable checkpoint. No blind retries.
    rows = "\n".join(json.dumps({"id": str(100 + index), "vector": [1, index]}) for index in range(5))
    control(page, "#job-file").set_input_files(files=[{"name": "rows.ndjson", "mimeType": "application/x-ndjson", "buffer": rows.encode()}])
    control(page, "#job-chunk-size").fill("2")
    control(page, "#job-prepare").click()
    expect(control(page, "#job-result")).to_contain_text("3")
    control(page, "#job-start").click()
    confirm(page, "into collection docs")
    expect(control(page, "#job-invalid")).to_contain_text("bad vector")
    expect(control(page, "#job-result")).to_contain_text("remain committed")
    assert [entry["path"] for entry in fake.log if entry["path"].startswith("/v1/imports/")] == ["/v1/imports/7/chunks/1", "/v1/imports/7/chunks/2"]
    assert fake.log[-3]["path"] == "/v1/collections/docs/imports" and fake.log[-3]["body"] is None
    control(page, "#job-resume").click()
    confirm(page, "Resume job 7 into collection docs")
    expect(control(page, "#job-progress")).to_have_attribute("value", "3")
    chunks = [entry["path"].rsplit("/", 1)[1] for entry in fake.log if "/chunks/" in entry["path"]]
    assert chunks == ["1", "2", "1", "2", "3"], chunks
    control(page, "#job-finish").click()
    confirm(page, "3 committed chunk(s) (5 rows) stay")
    expect(control(page, "#job-result")).to_contain_text("finished")
    assert fake.jobs["7"]["rows_committed"] == 5
    # Scoped credential: one-time secret is masked, never in the inspector, storage or URL.
    control(page, "#cred-name").fill("ci-reader")
    control(page, "#cred-create").click()
    confirm(page, "collections [docs]")
    expect(control(page, "#cred-secret")).to_contain_text("Shown once")
    expect(control(page, "#cred-table tbody tr")).to_have_count(1)
    expect(control(page, "#cred-table tbody")).to_contain_text("active")
    created = next(entry for entry in fake.log if entry["path"] == "/v1/admin/credentials" and entry["method"] == "POST")
    assert created["body"]["collections"] == ["docs"] and created["body"]["operations"] == ["read", "search"] and created["body"]["name"] == "ci-reader", created
    expect(control(page, "#cred-copy")).to_be_enabled()
    role(page, "button", name="Rotate cred00000001").click()
    confirm(page, "cred00000001")
    expect(control(page, "#cred-secret")).to_contain_text("cred00000002")
    state = visible_state(page)
    for token in fake.issued:
        assert token not in state and SECRET_TAIL[:20] not in state, "one-time secret leaked into page text, inputs, storage or URL"
    expect(control(page, "#inspector")).to_contain_text("/v1/admin/credentials")  # the follow-up list (no secrets) replaced it
    control(page, "#cred-clear").click()
    expect(control(page, "#cred-secret")).to_have_text("No new secret.")
    expect(control(page, "#cred-copy")).to_be_disabled()
    # Switching back (2 -> 3 dims) refills untouched example fields for the new dimensions.
    control(page, "#query").fill("[1,0]")
    control(page, "#filter").fill('[{"field":"category","op":"eq","value":{"type":"string","value":"x"}}]')
    control(page, "#collection").select_option("default")
    confirm(page, "Switch from collection docs to default")
    expect(control(page, "#collection-badge")).to_have_text("Collection: default")
    expect(control(page, "#record-vector")).to_have_value("[1,0,0]")
    expect(control(page, "#query")).to_have_value("[1,0,0]")
    # Disconnect clears collection-bound state.
    role(page, "button", name="Disconnect", exact=True).click()
    expect(control(page, "#collection-badge")).to_have_text("Collection: default")
    assert page.evaluate("Object.keys(localStorage).filter((key) => key !== 'vlite-appearance').length") == 0
    assert not errors, errors
    context.close()


def exercise_scoped_mocked(browser):
    scope = {"credential_id": "cred00000009", "collections": ["docs"], "operations": ["read", "search"], "expires_unix_ms": 4102444800000}
    fake = FakeApi(role="scoped", scope=scope)
    context, page, errors = mocked_page(browser, fake)
    expect(control(page, "#collection-badge")).to_have_text("Collection: docs")
    assert not any(entry["path"] in ("/v1/database", "/metrics") for entry in fake.log), "scoped principal must not read global counters"
    for selector in ["#delete-vector", "#collection-create", "#collection-drop", "#cred-create", "#job-start", "#build-index", "#quality-run", "#backup-schedule-save", "#prometheus"]:
        expect(control(page, selector)).to_be_disabled()
    assert option_disabled(page, "approximate")
    assert not option_disabled(page, "auto")
    expect(control(page, "#admin-note")).to_contain_text("operations read, search")
    control(page, "#query").fill("[1, 0]")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    assert fake.log[-1]["path"] == "/v1/collections/docs/search"
    assert not any(entry["method"] != "GET" and entry["path"] != "/v1/collections/docs/search" for entry in fake.log), fake.log
    assert not errors, errors
    context.close()


def exercise_legacy_mocked(browser):
    fake = FakeApi(features=["background_rebuild", "maintenance", "filtered_ann"], format_version=None)
    context, page, errors = mocked_page(browser, fake)
    expect(control(page, "#collection")).to_be_disabled()
    for selector in ["#collections-refresh", "#collection-create", "#job-start", "#cred-create", "#slow-queries", "#backup-verify", "#auto-maintenance-save"]:
        expect(control(page, selector)).to_be_disabled()
    for mode in ["auto", "hybrid"]:
        assert option_disabled(page, mode), mode
    expect(control(page, "#explain")).to_be_disabled()
    expect(control(page, "#collections-format")).to_contain_text("predates collections")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    assert fake.log[-1]["path"] == "/v1/search", fake.log[-1]
    assert not any(entry["path"].startswith("/v1/collections") for entry in fake.log), fake.log
    assert not errors, errors
    context.close()


def exercise_settings_routes_mocked(browser):
    """Settings-sensitive calls use the selected collection's route; nothing silently targets default."""
    fake = FakeApi()
    context, page, errors = mocked_page(browser, fake)
    # Default on a newer server: collection route so default's stored settings apply (legacy routes use hard defaults).
    assert any(entry["path"] == "/v1/collections/default/index/hnsw" for entry in fake.log), fake.log
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    assert fake.log[-1]["path"] == "/v1/collections/default/search", fake.log[-1]
    assert fake.log[-1]["body"]["metric"] == "squared_l2" and fake.log[-1]["body"]["config"]["ef_construction"] == 200, fake.log[-1]
    control(page, "#read-maintenance").click()
    expect(control(page, "#maintenance-result")).to_contain_text('"collection": "default"')
    assert fake.log[-1]["path"] == "/v1/collections/default/index/hnsw/maintenance", fake.log[-1]
    expect(control(page, "#maintenance-scope")).to_contain_text("legacy route")
    control(page, "#save-maintenance").click()
    confirm(page, "Thresholds apply to every collection")
    expect(control(page, "#status")).to_contain_text("PUT /v1/index/hnsw/maintenance: HTTP 200")
    # Named collection: read/tick on its own route; the legacy-only policy save is disabled and explained.
    control(page, "#collection").select_option("docs")
    confirm(page, "Switch from collection default to docs")
    expect(control(page, "#collection-badge")).to_have_text("Collection: docs")
    switched = len(fake.log)
    expect(control(page, "#metric")).to_have_value("cosine")
    expect(control(page, "#save-maintenance")).to_be_disabled()
    expect(control(page, "#maintenance-scope")).to_contain_text("Collection docs")
    control(page, "#read-maintenance").click()
    expect(control(page, "#maintenance-result")).to_contain_text('"collection": "docs"')
    expect(control(page, "#maintenance-policy")).to_have_value(re.compile(r'"metric": "cosine"'))
    control(page, "#tick-maintenance").click()
    confirm(page, "collection docs")
    expect(control(page, "#status")).to_contain_text("POST /v1/collections/docs/index/hnsw/maintenance/tick: HTTP 200")
    control(page, "#query").fill("[1, 0]")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    after = [entry["path"] for entry in fake.log[switched - 1:]]
    assert "/v1/collections/docs/index/hnsw/maintenance" in after and "/v1/collections/docs/search" in after, after
    assert not any(path.startswith(("/v1/index", "/v1/search", "/v1/vectors", "/v1/batches", "/v1/collections/default/")) for path in after), after
    assert fake.log[-1]["body"]["metric"] == "cosine", fake.log[-1]
    # A ready index built with an explicit identity (m=8, ef=40) that differs from stored settings: on switch the form
    # follows the ready index (so ready-only/auto searches hit it) and says so.
    fake.ready_index = {"default": {"config": {"m": 8, "ef_construction": 40, "seed": "0"}, "metric": "squared_l2"}}
    control(page, "#collection").select_option("default")
    confirm(page, "Switch from collection docs to default")
    expect(control(page, "#index-status")).to_contain_text("differs from the stored settings")
    expect(control(page, "#hnsw-m")).to_have_value("8")
    expect(control(page, "#hnsw-ef")).to_have_value("40")
    expect(control(page, "#metric")).to_have_value("squared_l2")
    role(page, "button", name="Search", exact=True).click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    assert fake.log[-1]["path"] == "/v1/collections/default/search" and fake.log[-1]["body"]["config"]["m"] == 8, fake.log[-1]
    assert not errors, errors
    context.close()

def exercise_operations_mocked(browser):
    fake = FakeApi()
    context, page, errors = mocked_page(browser, fake)
    # Schedulers are off by default; saves are partial merges with an explicit confirmation.
    control(page, "#backup-schedule-read").click()
    expect(control(page, "#backup-schedule")).to_have_value(json.dumps(fake.schedule, indent=2).replace(": False", ": false").replace(": True", ": true"))
    control(page, "#backup-schedule").fill('{"retention_enabled": true, "retain_last": 3}')
    control(page, "#backup-schedule-save").click()
    confirm(page, "RETENTION ON", accept=False)
    assert fake.schedule["retention_enabled"] is False
    control(page, "#backup-schedule-save").click()
    confirm(page, "beyond the newest 3")
    expect(control(page, "#status")).to_contain_text("PUT /v1/admin/backups/schedule: HTTP 200")
    assert fake.log[-1]["method"] == "PUT" and fake.log[-1]["body"] == {"retention_enabled": True, "retain_last": 3}, fake.log[-1]
    control(page, "#auto-maintenance-read").click()
    expect(control(page, "#auto-maintenance")).to_have_value(re.compile(r'"interval_secs": 60'))
    control(page, "#auto-maintenance").fill('{"enabled": true, "interval_secs": 30}')
    control(page, "#auto-maintenance-save").click()
    confirm(page, "ENABLE automatic maintenance")
    expect(control(page, "#status")).to_contain_text("PUT /v1/admin/maintenance/auto: HTTP 200")
    assert fake.auto["enabled"] is True and fake.auto["interval_ms"] == 30000
    control(page, "#auto-maintenance").fill('{"interval_secs": 5}')
    control(page, "#auto-maintenance-save").click()
    expect(control(page, "#status")).to_contain_text("interval_secs")
    expect(control(page, "#confirmation")).to_be_hidden()
    control(page, "#auto-maintenance-tick").click()
    confirm(page, "exactly one bounded")
    expect(control(page, "#status")).to_contain_text("POST /v1/admin/maintenance/auto/tick: HTTP 200")
    control(page, "#backup-schedule-run").click()
    confirm(page, "Run one scheduled backup cycle now", accept=False)
    assert fake.log[-1]["path"] != "/v1/admin/backups/schedule/run"
    control(page, "#backup-schedule-run").click()
    confirm(page, "retention deletes only when enabled")
    expect(control(page, "#backup-schedule-result")).to_contain_text("vlite-backup-1.vlite")
    # Diagnostics: bounded slow-query table (no vectors/text) and Prometheus text.
    control(page, "#slow-queries").click()
    expect(control(page, "#slow-summary")).to_contain_text("threshold 250 ms")
    expect(control(page, "#slow-table tbody tr")).to_have_count(1)
    expect(control(page, "#slow-table tbody")).to_contain_text("exact_scan")
    control(page, "#prometheus").click()
    expect(control(page, "#prometheus-result")).to_contain_text("vlite_records 2")
    # Indeterminate credential commit: reported as IN EFFECT (never "no change"), list re-read, no secret shown.
    fake.indeterminate = True
    control(page, "#cred-name").fill("ci-indeterminate")
    control(page, "#cred-create").click()
    confirm(page, "Create a credential")
    expect(control(page, "#cred-result")).to_contain_text("IS in effect")
    expect(control(page, "#cred-table tbody")).to_contain_text("ci-indeterminate")
    expect(control(page, "#cred-secret")).to_have_text("No new secret.")
    expect(control(page, "#error-help")).to_contain_text("Do not assume nothing changed")
    assert not any(token in visible_state(page) for token in fake.issued)
    # New sections keep the existing narrow layout constraint.
    page.set_viewport_size({"width": 390, "height": 844})
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")
    assert not errors, errors
    context.close()

class FeatureApi(FakeApi):
    def __init__(self, role="admin", scope=None):
        super().__init__(role=role, format_version=5, scope=scope, features=ALL_FEATURES + [
            "record_expiry", "metadata_updates", "grouped_search", "geo_search", "recommendations",
            "facets", "numeric_stats", "named_vectors", "sparse_vectors", "change_feed", "webhooks", "disk_ann"])
        self.caps["routes"] = ["/v1/collections/{collection}/query"]
        self.hooks = []

    def api(self, method, path, query, body, headers):
        if path == "/v1/database":
            return 200, {"dimensions": 3, "records": 1, "is_encrypted": True, "encryption_key_id": "18446744073709551615"}, {}
        if path == "/v1/changes":
            from urllib.parse import parse_qs
            cursor = parse_qs(query).get("cursor", [""])[0]
            return 200, {"changes": [{"sequence": "18446744073709551615", "collection": "docs", "change": {"type": "put", "id": "5"}}],
                         "head": "18446744073709551615", "next_cursor": "a+b/=&?", "has_more": not cursor}, {}
        if path == "/v1/admin/restores":
            return 201, {"name": "vlite-restored-1.vlite", "restart_required": False}, {}
        if path == "/v1/admin/webhooks":
            if method == "POST":
                self.hooks.append({"id": "a" * 24, "url": body["url"], "attempts": 2, "last_error": "receiver unavailable", "last_success_unix_ms": None})
                return 201, self.hooks[-1], {}
            return 200, {"subscriptions": self.hooks}, {}
        if path.startswith("/v1/admin/webhooks/"):
            if method == "DELETE":
                self.hooks.clear()
                return 200, {"deleted": True}, {}
            return 200, self.hooks[0], {}
        collection, rest = self.collection_route(path)
        if rest.endswith("/metadata") and method == "PATCH":
            key = (collection, rest.split("/")[2])
            if headers.get("if-match") != self.etag(*key):
                return self.error(412, "precondition_failed")
            self.records[key]["revision"] += 1
            return 200, {"updated": True}, {}
        if rest == "/index/disk-ann":
            return 200, {"collection_id": 1, "sequence": "44", "path_name": "disk-ann-1.dann"}, {}
        if rest in ["/recommend", "/search/named", "/search/dense-sparse", "/search/geo", "/search/disk-ann"]:
            return 200, {"hits": [{"id": "5", "distance": 0, "score": 0.032}]}, {}
        if rest == "/search/grouped":
            return 200, {"groups": [{"value": {"type": "string", "value": "<script>unsafe</script>"}, "hits": [{"id": "5", "distance": 0}]}]}, {}
        if rest == "/facets":
            return 200, {"facets": [{"value": {"type": "string", "value": "docs"}, "count": 1}]}, {}
        if rest == "/stats/numeric":
            return 200, {"count": 1, "min": 2, "max": 2, "sum": 2, "average": 2}, {}
        if rest == "/query":
            return 200, {"hits": [{"id": "5", "distance": 0}], "groups": [], "facets": []}, {}
        return super().api(method, path, query, body, headers)


def exercise_features_mocked(browser):
    fake = FeatureApi()
    context, page, errors = mocked_page(browser, fake)
    expect(control(page, "#encryption-badge")).to_contain_text("Encrypted (key ID 18446744073709551615)")
    control(page, "#id").fill("1")
    control(page, "#get-vector").click()
    expect(control(page, "#record-revision")).to_contain_text("revision 1")
    control(page, "#record-ttl").fill("60000")
    control(page, "#vector-form button[type=submit]").click()
    confirm(page, "record 1 in collection default")
    expect(control(page, "#record")).to_contain_text('"committed": true')
    assert fake.log[-1]["body"]["ttl_ms"] == 60000
    control(page, "#get-vector").click()
    expect(control(page, "#record-revision")).to_contain_text("revision 2")
    control(page, "#metadata").fill('{"label":{"type":"string","value":"edited"}}')
    control(page, "#patch-metadata").click()
    confirm(page, "Replace only metadata")
    expect(control(page, "#record")).to_contain_text('"metadata_updated": true')
    patch = fake.log[-1]
    assert patch["method"] == "PATCH" and patch["path"] == "/v1/vectors/1/metadata"
    assert "if-match" in patch["headers"] and list(patch["body"]) == ["metadata"]
    control(page, "#collection").select_option("docs")
    confirm(page, "unsaved record editor changes")
    # The switch resets the search form after its confirmation round-trip; wait so it cannot erase the inputs below.
    expect(control(page, "#status")).to_contain_text("Collection docs selected")
    control(page, "#query").fill("[1,0]")
    control(page, "#rerank-mode").select_option("mmr")
    control(page, "#mmr-lambda").fill("0.25")
    control(page, "#search-form button[type=submit]").click()
    expect(control(page, "#hits tbody tr")).to_have_count(1)
    searches = [entry for entry in fake.log if entry["method"] == "POST" and entry["path"] == "/v1/collections/docs/search"]
    assert searches[-1]["body"]["rerank"] == {"mode": "mmr", "candidates": 50, "lambda": .25}
    cases = [("group-search", "group-result", "/search/grouped"), ("geo-search", "geo-result", "/search/geo"),
             ("facets-run", "facets-result", "/facets"), ("stats-run", "stats-result", "/stats/numeric")]
    for button, result, suffix in cases:
        control(page, "#" + button).click()
        expect(control(page, "#" + result)).not_to_have_text("Request pending")
        assert fake.log[-1]["path"] == "/v1/collections/docs" + suffix
        assert fake.log[-1]["body"]["filter"] == []
    expect(control(page, "#group-result")).to_contain_text("<script>unsafe</script>")
    expect(control(page, "#group-result script")).to_have_count(0)
    control(page, "#named-query").fill("[0,1,0,0]")  # named dimensions need not match the collection
    control(page, "#named-version").fill("model-v2")
    control(page, "#sparse-query").fill("[[1,1]]")
    for button, suffix in [("named-search", "/search/named"), ("dense-sparse-search", "/search/dense-sparse")]:
        control(page, "#" + button).click()
        expect(control(page, "#named-result")).to_contain_text('"hits"')
        assert fake.log[-1]["path"] == "/v1/collections/docs" + suffix
        assert fake.log[-1]["body"]["version"] == "model-v2"
    control(page, "#recommend-positive").fill("18446744073709551615")
    control(page, "#recommend-run").click()
    expect(control(page, "#recommend-result")).to_contain_text('"hits"')
    assert fake.log[-1]["body"]["positive_ids"] == ["18446744073709551615"]
    control(page, "#language-vectors").fill('{"q":[1,0]}')
    control(page, "#language-run").click()
    expect(control(page, "#language-result")).to_contain_text('"facets"')
    assert fake.log[-1]["body"]["vectors"] == {"q": [1, 0]}
    control(page, "#disk-build").click()
    confirm(page, "Build disk ANN for collection docs")
    expect(control(page, "#disk-result")).to_contain_text("disk-ann-1.dann")
    control(page, "#disk-probe").click()
    expect(control(page, "#disk-result")).to_contain_text('"hits"')
    assert fake.log[-1]["body"] == {"vector": [1, 0], "k": 1, "ef_search": 100}
    control(page, "#feed-load").click()
    expect(control(page, "#feed-result")).to_contain_text("18446744073709551615")
    control(page, "#feed-next").click()
    expect(control(page, "#feed-next")).to_be_disabled()
    assert "cursor=a%2Bb%2F%3D%26%3F" in fake.log[-1]["query"]
    control(page, "#webhook-url").fill("https://consumer.example/hook")
    control(page, "#webhook-secret").fill("cancelled-signing-secret")
    before = len(fake.log)
    control(page, "#webhook-create").click()
    confirm(page, "Create this webhook", accept=False)
    expect(control(page, "#webhook-secret")).to_have_value("")
    assert len(fake.log) == before and "cancelled-signing-secret" not in visible_state(page)
    control(page, "#webhook-secret").fill("local-webhook-signing-secret")
    control(page, "#webhook-create").click()
    confirm(page, "Create this webhook")
    expect(control(page, "#webhook-result")).to_contain_text("receiver unavailable")
    expect(control(page, "#webhook-secret")).to_have_value("")
    assert "local-webhook-signing-secret" not in visible_state(page)
    control(page, "#webhook-list").click()
    expect(control(page, "#webhook-result")).to_contain_text('"attempts": 2')
    control(page, "#webhook-id").fill("a" * 24)
    control(page, "#webhook-status").click()
    expect(control(page, "#webhook-result")).to_contain_text('"last_error"')
    control(page, "#webhook-delete").click()
    confirm(page, "Delete webhook")
    expect(control(page, "#webhook-result")).to_contain_text('"subscriptions": []')
    control(page, "#pitr-backup").fill("vlite-backup-1.vlite")
    control(page, "#pitr-target").fill("42")
    control(page, "#pitr-run").click()
    confirm(page, "at sequence 42")
    expect(control(page, "#pitr-result")).to_contain_text("vlite-restored-1.vlite")
    assert fake.log[-1]["body"] == {"backup": "vlite-backup-1.vlite", "sequence": 42}
    assert not any(item["path"].endswith("/select") for item in fake.log)
    control(page, "#disconnect").click()
    for name in ["group-result", "named-result", "geo-result", "recommend-result", "facets-result", "stats-result", "language-result", "disk-result", "feed-result", "webhook-result", "pitr-result"]:
        expect(control(page, "#" + name)).to_have_text("")
    assert not errors, errors
    context.close()
    # Read-only retrieval POSTs are not mistaken for mutations; admin actions stay gated.
    for credential_role, scope in [("read_only", None), ("scoped", {"collections": ["docs"], "operations": ["read", "search"], "expires_unix_ms": 2000000000000})]:
        fake = FeatureApi(credential_role, scope)
        context, page, errors = mocked_page(browser, fake)
        for name in ["patch-metadata", "webhook-create", "webhook-list", "pitr-run", "disk-build"]:
            expect(control(page, "#" + name)).to_be_disabled()
        if scope:
            expect(control(page, "#feed-load")).to_be_disabled()
        control(page, "#facets-run").click()
        expect(control(page, "#facets-result")).to_contain_text('"facets"')
        assert not errors, errors
        context.close()
    print("PASS V5 console: TTL, metadata If-Match, MMR, grouped/geo/named/dense+sparse/recommend/facets/stats/query, cursor paging, webhooks secret hygiene, disk ANN, PITR, encryption and role gates")


def exercise_epic015_mocked(browser):
    exercise_navigation_mocked(browser)
    exercise_read_loading_mocked(browser)
    exercise_features_mocked(browser)
    exercise_collections_mocked(browser)
    exercise_scoped_mocked(browser)
    exercise_legacy_mocked(browser)
    exercise_settings_routes_mocked(browser)
    exercise_operations_mocked(browser)
    print("PASS EPIC-015 mocked console: collection switch confirm/cancel isolation, If-Match 412, hybrid/auto rendering, resumable import resume/replay, one-time masked credentials, scoped and legacy gating, scheduler merges/confirmations and run-now, selected-collection maintenance/search routing, diagnostics")

def main():
    name = "vlite-dashboard-" + uuid.uuid4().hex[:10]
    volume = name + "-data"
    network = name + "-network"
    ui_name = name + "-ui"
    v4_name, v4_ui, v4_network = name + "-v4", name + "-v4-ui", name + "-v4-network"
    image = os.environ.get("VLITE_IMAGE", "vlite-dashboard:test")
    ui_image = os.environ.get("VLITE_DASHBOARD_IMAGE", "vlite-dashboard-ui:test")
    token = secrets.token_urlsafe(32)
    read_token = secrets.token_urlsafe(32)
    if not os.environ.get("VLITE_E2E_NO_BUILD"):
        docker("build", "--target", "runtime", "-t", image, ".")
        docker("build", "--target", "dashboard", "-t", ui_image, ".")
    docker("volume", "create", volume)
    docker("network", "create", network)
    try:
        # File secret: no token in process arguments or environment.
        with tempfile.TemporaryDirectory(prefix="vlite-ui-secret-") as directory:
            secret = os.path.join(directory, "token")
            with open(secret, "w", encoding="ascii") as stream:
                stream.write(token)
            os.chmod(secret, 0o644)
            read_secret = os.path.join(directory, "read-token")
            with open(read_secret, "w", encoding="ascii") as stream:
                stream.write(read_token)
            os.chmod(read_secret, 0o644)
            admin_dir = os.path.join(directory, "admin")
            os.mkdir(admin_dir)
            os.chmod(admin_dir, 0o777)
            docker("run", "-d", "--name", name, "--network", network, "--network-alias", "api",
                   "--mount", f"type=volume,source={volume},target=/data",
                   "--mount", f"type=bind,source={secret},target=/run/token,readonly",
                   "--mount", f"type=bind,source={read_secret},target=/run/read-token,readonly",
                   "--mount", f"type=bind,source={admin_dir},target=/admin",
                   "-e", "VLITE_READ_ONLY_TOKEN_FILE=/run/read-token", "-e", "VLITE_ADMIN_DIR=/admin",
                   "-p", "127.0.0.1::8080", "--entrypoint", "vlite-server", image,
                   "/data/vectors.vlite", "--create-if-missing", "3",
                   "--bind", "0.0.0.0:8080", "--allow-remote", "--token-file", "/run/token")
            docker("run", "-d", "--name", ui_name, "--network", network,
                   "-p", "127.0.0.1::8080", ui_image)
            port = docker("port", name, "8080/tcp").rsplit(":", 1)[1]
            ui_port = docker("port", ui_name, "8080/tcp").rsplit(":", 1)[1]
            with sync_playwright() as playwright:
                browser = playwright.chromium.launch()
                try:
                    page = browser.new_page()
                    page.set_default_timeout(30000)
                    base = "http://127.0.0.1:" + port
                    ready(page, base)
                    exercise_layout(page, base + "/dashboard")
                    exercise(page, base, token)
                    ui_base = "http://127.0.0.1:" + ui_port
                    ready(page, ui_base + "/api")
                    exercise_layout(page, ui_base + "/")
                    exercise_separate_app(page, ui_base, token)
                    docker("restart", name)
                    # Docker may reassign an ephemeral host port across restart.
                    port = docker("port", name, "8080/tcp").rsplit(":", 1)[1]
                    base = "http://127.0.0.1:" + port
                    ready(page, base)
                    page.goto(base + "/dashboard")
                    control(page, "#token").fill(token)
                    role(page, "button", name="Connect / refresh").click()
                    expect(control(page, "#status")).to_have_text("Connected; overview refreshed")
                    expect(control(page, "#index-status")).to_contain_text("Not ready")
                    control(page, "#id").fill("2")
                    role(page, "button", name="Get", exact=True).click()
                    expect(control(page, "#record")).to_contain_text('"payload_base64": "AP8="')
                    role(page, "button", name="Disconnect", exact=True).click()
                    # Fresh page has no bearer credential; every API still fails closed.
                    assert page.request.get(base + "/v1/database").status == 401
                    ready(page, ui_base + "/api")
                    exercise_separate_app(page, ui_base, token, "Not ready")
                    exercise_console_tools(page, ui_base, token)
                    exercise_operations(page, ui_base, token, read_token)
                    exercise_epic015_real(page, ui_base, token)
                    # Format v4 stack: the same console against a real v4 API (collections, imports, credentials).
                    v4_admin = os.path.join(directory, "admin-v4")
                    os.mkdir(v4_admin)
                    os.chmod(v4_admin, 0o777)
                    docker("network", "create", v4_network)
                    docker("run", "-d", "--name", v4_name, "--network", v4_network, "--network-alias", "api",
                           "--mount", f"type=bind,source={secret},target=/run/token,readonly",
                           "--mount", "type=tmpfs,target=/data,tmpfs-mode=1777",
                           "--mount", f"type=bind,source={v4_admin},target=/admin",
                           "-e", "VLITE_ADMIN_DIR=/admin", "--entrypoint", "vlite-server", image,
                           "/data/v4.vlite", "--create-if-missing", "3", "--create-format", "4",
                           "--bind", "0.0.0.0:8080", "--allow-remote", "--token-file", "/run/token")
                    docker("run", "-d", "--name", v4_ui, "--network", v4_network, "-p", "127.0.0.1::8080", ui_image)
                    v4_base = "http://127.0.0.1:" + docker("port", v4_ui, "8080/tcp").rsplit(":", 1)[1]
                    ready(page, v4_base + "/api")
                    v4_caps = page.request.get(v4_base + "/api/v1/capabilities", headers={"Authorization": "Bearer " + token}).json()
                    assert v4_caps.get("format_version") == 4 and v4_caps["features"].get("collections"), v4_caps
                    exercise_epic015_real(page, v4_base, token)
                    exercise_epic015_mocked(browser)
                    print("PASS Chromium dashboards: embedded REST UI and separately containerized REST-proxy UI; auth, CRUD, pagination, search, index readiness, confirmation, token lifecycle and API restart persistence")
                finally:
                    browser.close()
    finally:
        subprocess.run(["docker", "rm", "-f", ui_name, name, v4_ui, v4_name], check=False, capture_output=True, timeout=60)
        subprocess.run(["docker", "network", "rm", network], check=False, capture_output=True, timeout=60)
        subprocess.run(["docker", "network", "rm", v4_network], check=False, capture_output=True, timeout=60)
        print("Retained Docker data volume:", volume)


if __name__ == "__main__":
    import sys
    if "--mocked" in sys.argv:
        with sync_playwright() as playwright:
            mocked_browser = playwright.chromium.launch()
            try:
                exercise_epic015_mocked(mocked_browser)
            finally:
                mocked_browser.close()
    else:
        main()
