"""Real Chromium UI tests against an isolated Docker API; keeps its named data volume."""

import json
import os
import secrets
import subprocess
import tempfile
import time
import uuid
from playwright.sync_api import Error as PlaywrightError, sync_playwright, expect


def docker(*args, **kwargs):
    result = subprocess.run(["docker", *args], capture_output=True, text=True, timeout=600, **kwargs)
    if result.returncode:
        raise RuntimeError(f"Docker {args[0]} failed ({result.returncode}):\n{result.stdout[-12000:]}\n{result.stderr[-12000:]}")
    return result.stdout.strip()

def ready(page, base):
    deadline = time.monotonic() + 30
    last_error = None
    while time.monotonic() < deadline:
        try:
            response = page.request.get(base + "/health", timeout=1000)
            if response.status == 200:
                return
        except PlaywrightError as error:
            last_error = error
        time.sleep(.1)
    raise RuntimeError("Dashboard API did not become ready within 30 seconds") from last_error


def exercise(page, base, token):
    errors = []
    assets = {}
    page.on("pageerror", lambda error: errors.append(str(error)))
    page.on("response", lambda response: assets.update({response.url: response.status}) if "/dashboard/" in response.url else None)
    page.goto(base + "/dashboard")
    assert assets.get(base + "/dashboard/app.js") == 200, assets
    assert assets.get(base + "/dashboard/style.css") == 200, assets
    assert not errors, errors
    expect(page.get_by_role("heading", name="Connection", exact=True)).to_be_visible()
    expect(page.locator("#status")).to_have_text("Disconnected")
    page.locator("#token").fill("x" * 32)
    page.get_by_role("button", name="Connect / refresh").click()
    expect(page.locator("#status")).to_contain_text("HTTP 401")
    page.locator("#token").fill(token)
    page.get_by_role("button", name="Connect / refresh").click()
    expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
    expect(page.locator("#token")).to_have_value("")
    expect(page.locator("#overview")).to_contain_text('"dimensions": 3')
    expect(page.locator("#connection-badge")).to_have_text("Connected")
    expect(page.locator("#stat-records")).to_have_text("0")
    expect(page.locator("#stat-dimensions")).to_have_text("3")
    page.get_by_role("button", name="Fill example", exact=True).click()
    expect(page.locator("#record-vector")).to_have_value("[1,0,0]")
    assert json.loads(page.locator("#metadata").input_value())["category"]["value"] == "demo"
    expect(page.locator("#status")).to_contain_text("nothing saved")
    page.get_by_role("button", name="Use editor vector as query", exact=True).click()
    expect(page.locator("#query")).to_have_value("[1,0,0]")
    expect(page.locator("#index-status")).to_contain_text("Not ready")
    page.get_by_role("button", name="First page / refresh", exact=True).click()
    expect(page.locator("#browse-status")).to_contain_text("0 records")
    expect(page.locator("#next-page")).to_be_disabled()
    page.get_by_role("button", name="Connect / refresh").click()
    expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
    page.locator("#id").fill("18446744073709551615")
    page.locator("#record-vector").fill("[1,0,0]")
    page.locator("#payload").fill("AP8=")
    # Untrusted data must be rendered as text, never injected HTML.
    metadata = {"category": {"type": "string", "value": "<img src=x onerror=alert(1)>"}}
    page.locator("#metadata").fill(json.dumps(metadata))
    page.get_by_role("button", name="Put / replace").click()
    expect(page.locator("#confirmation")).to_be_visible()
    page.get_by_role("button", name="Cancel", exact=True).click()
    page.get_by_role("button", name="Get", exact=True).click()
    expect(page.locator("#status")).to_contain_text("HTTP 404")
    page.get_by_role("button", name="Put / replace").click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#record")).to_contain_text('"committed": true')
    expect(page.locator("#stat-records")).to_have_text("--")
    page.get_by_role("button", name="First page / refresh", exact=True).click()
    expect(page.locator("#vectors tbody tr")).to_have_count(1)
    expect(page.locator("#vectors tbody")).to_contain_text("<img src=x onerror=alert(1)>")
    expect(page.locator("#vectors img")).to_have_count(0)
    page.get_by_role("button", name="18446744073709551615", exact=True).click()
    expect(page.locator("#record")).to_contain_text('"payload_base64": "AP8="')
    page.get_by_role("button", name="Get", exact=True).click()
    expect(page.locator("#record")).to_contain_text("<img src=x onerror=alert(1)>")
    expect(page.locator("#record img")).to_have_count(0)
    page.locator("#query").fill("[1,0,0]")
    page.locator("#filter").fill(json.dumps([{"op": "eq", "field": "category", "value": metadata["category"]}]))
    page.get_by_role("button", name="Search", exact=True).click()
    expect(page.locator("#hits tbody tr")).to_have_count(1)
    expect(page.locator("#hits tbody")).to_contain_text("18446744073709551615")
    expect(page.locator("#inspector")).to_contain_text('"distance": 0')
    for metric, distance in [("cosine", 0), ("inner_product", -1)]:
        page.locator("#metric").select_option(metric)
        page.get_by_role("button", name="Search", exact=True).click()
        expect(page.locator("#inspector")).to_contain_text(f'"metric": "{metric}"')
        expect(page.locator("#inspector")).to_contain_text(f'"distance": {distance}')
        expect(page.locator("#hits caption")).to_contain_text(metric)
    page.locator("#metric").select_option("squared_l2")
    page.locator("#mode").select_option("approximate_ready")
    expect(page.locator("#metric")).to_be_enabled()
    expect(page.locator('#metric option[value="inner_product"]')).to_have_js_property("disabled", True)
    expect(page.locator("#filter")).to_be_disabled()
    expect(page.locator("#ef")).to_be_enabled()
    page.get_by_role("button", name="Search", exact=True).click()
    expect(page.locator("#status")).to_contain_text("index_not_ready")
    expect(page.locator("#hits tbody tr")).to_have_count(0)
    page.get_by_role("button", name="Build HNSW", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#build-result")).to_contain_text("Build completed")
    expect(page.locator("#index-status")).to_contain_text("Ready;")
    page.get_by_role("button", name="Search", exact=True).click()
    expect(page.locator("#hits tbody tr")).to_have_count(1)
    page.locator("#metric").select_option("cosine")
    page.get_by_role("button", name="Search", exact=True).click()
    expect(page.locator("#status")).to_contain_text("index_not_ready")
    page.get_by_role("button", name="Build HNSW", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#index-status")).to_contain_text("metric cosine")
    page.get_by_role("button", name="Search", exact=True).click()
    expect(page.locator("#hits tbody tr")).to_have_count(1)
    expect(page.locator("#hits caption")).to_contain_text("cosine")
    page.locator("#metric").select_option("squared_l2")
    page.locator("#mode").select_option("approximate")
    page.get_by_role("button", name="Search", exact=True).click()
    expect(page.locator("#inspector")).to_contain_text('"mode": "approximate"')
    # Client-side validation must not issue a malformed request.
    page.locator("#id").fill("01")
    page.get_by_role("button", name="Get", exact=True).click()
    expect(page.locator("#status")).to_contain_text("canonical decimal u64")
    page.locator("#id").fill("18446744073709551615")
    page.get_by_role("button", name="Delete", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#record")).to_contain_text('"deleted": true')
    expect(page.locator("#inspector")).not_to_contain_text(token)
    # Leave a distinct durable record for the restart check.
    page.locator("#id").fill("2")
    page.get_by_role("button", name="Put / replace").click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#record")).to_contain_text('"committed": true')
    # Two pages, numerically ordered beyond JavaScript's safe-integer range.
    page.locator("#id").fill("18446744073709551615")
    page.get_by_role("button", name="Put / replace").click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#record")).to_contain_text('"committed": true')
    page.locator("#page-size").fill("1")
    page.get_by_role("button", name="First page / refresh", exact=True).click()
    expect(page.locator("#vectors tbody")).to_contain_text("2")
    expect(page.locator("#vectors tbody")).not_to_contain_text("18446744073709551615")
    expect(page.locator("#next-page")).to_be_enabled()
    page.get_by_role("button", name="Next page", exact=True).click()
    expect(page.locator("#vectors tbody")).to_contain_text("18446744073709551615")
    expect(page.locator("#next-page")).to_be_disabled()
    page.get_by_role("button", name="First page / refresh", exact=True).click()
    expect(page.locator("#next-page")).to_be_enabled()
    page.locator("#id").fill("2")
    page.get_by_role("button", name="Put / replace").click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#vectors tbody tr")).to_have_count(0)
    expect(page.locator("#next-page")).to_be_disabled()
    expect(page.locator("#index-status")).to_contain_text("Data changed")
    page.get_by_role("button", name="Refresh index status", exact=True).click()
    expect(page.locator("#index-status")).to_contain_text("Ready; metric squared_l2; buffer 2/256")
    assert page.evaluate("localStorage.length + sessionStorage.length") == 0
    page.get_by_role("button", name="Disconnect", exact=True).click()
    expect(page.locator("#overview")).to_have_text("")
    expect(page.locator("#inspector")).to_have_text("")
    expect(page.locator("#metadata")).to_have_value("{}")
    expect(page.locator("#vectors tbody tr")).to_have_count(0)
    expect(page.locator("#index-status")).to_contain_text("not checked")
    expect(page.locator("#connection-badge")).to_have_text("Disconnected")
    expect(page.locator("#stat-dimensions")).to_have_text("--")
    page.reload()
    expect(page.locator("#status")).to_have_text("Disconnected")
    page.get_by_role("button", name="Get", exact=True).click()
    expect(page.locator("#status")).to_contain_text("Connect before")
    assert not errors, errors
    # Responsive smoke check.
    page.set_viewport_size({"width": 390, "height": 844})
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")

def exercise_separate_app(page, base, token, expected_index="Ready;"):
    errors = []
    api_requests = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    page.on("request", lambda request: api_requests.append(request.url) if "/api/" in request.url else None)
    page.goto(base + "/")
    expect(page.get_by_role("heading", name="Connection", exact=True)).to_be_visible()
    expect(page.locator("#status")).to_have_text("Disconnected")
    page.locator("#token").fill("x" * 32)
    page.get_by_role("button", name="Connect / refresh").click()
    expect(page.locator("#status")).to_contain_text("HTTP 401")
    page.locator("#token").fill(token)
    page.get_by_role("button", name="Connect / refresh").click()
    expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
    expect(page.locator("#overview")).to_contain_text('"dimensions": 3')
    expect(page.locator("#stat-records")).to_have_text("2")
    expect(page.locator("#stat-dimensions")).to_have_text("3")
    page.get_by_role("navigation", name="Dashboard navigation").get_by_role("link", name="Search playground", exact=True).click()
    assert page.url.endswith("#search")
    expect(page.locator("#index-status")).to_contain_text(expected_index)
    page.get_by_role("button", name="First page / refresh", exact=True).click()
    expect(page.locator("#vectors tbody tr")).to_have_count(2)
    page.locator("#query").fill("[1,0,0]")
    page.locator("#k").fill("2")
    page.get_by_role("button", name="Search", exact=True).click()
    expect(page.locator("#hits tbody tr")).to_have_count(2)
    expect(page.locator("#inspector")).to_contain_text('"distance": 0')
    expect(page.locator("#search-summary")).to_contain_text("2 neighbors returned")
    page.locator("#hits tbody button").first.click()
    expect(page.locator("#record")).to_contain_text('"payload_base64": "AP8="')
    expect(page.locator("#id")).to_have_value("2")
    assert any(url.endswith("/api/v1/database") for url in api_requests), api_requests
    assert any(url.endswith("/api/v1/search") for url in api_requests), api_requests
    assert page.evaluate("localStorage.length + sessionStorage.length") == 0
    page.get_by_role("button", name="Disconnect", exact=True).click()
    expect(page.locator("#overview")).to_have_text("")
    assert not errors, errors


def exercise_console_tools(page, base, token):
    page.goto(base + "/")
    page.locator("#token").fill(token)
    page.get_by_role("button", name="Connect / refresh").click()
    expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
    expect(page.locator("#compatibility")).to_contain_text("version")
    page.locator("#theme").select_option("dark")
    page.locator("#density").select_option("compact")
    assert page.evaluate("document.documentElement.dataset.theme") == "dark"
    assert page.evaluate("localStorage.length + sessionStorage.length") == 0
    page.locator("#remember-preferences").check()
    assert page.evaluate("JSON.parse(localStorage.getItem('vlite-appearance')).theme") == "dark"
    page.locator("#remember-preferences").uncheck()
    assert page.evaluate("localStorage.length + sessionStorage.length") == 0
    page.get_by_role("button", name="Prepare 3 demo vectors", exact=True).click()
    expect(page.locator("#import-result")).to_contain_text("Preview only")
    page.get_by_role("button", name="Commit previewed records", exact=True).click()
    expect(page.locator("#confirmation")).to_be_visible()
    page.get_by_role("button", name="Cancel", exact=True).click()
    expect(page.locator("#stat-records")).to_have_text("2")
    page.locator("#import-file").set_input_files({
        "name": "invalid.json", "mimeType": "application/json",
        "buffer": b'[{"id":"01","vector":[1,0,0]}]',
    })
    page.get_by_role("button", name="Validate / preview import", exact=True).click()
    expect(page.locator("#status")).to_contain_text("canonical decimal")
    records = [
        {"id": "101", "vector": [1, 0, 0], "metadata": {"category": {"type": "string", "value": "demo"}}},
        {"id": "102", "vector": [0, 1, 0], "metadata": {"category": {"type": "integer", "value": "42"}}},
    ]
    page.locator("#import-file").set_input_files({
        "name": "records.json", "mimeType": "application/json",
        "buffer": json.dumps(records).encode(),
    })
    page.get_by_role("button", name="Validate / preview import", exact=True).click()
    expect(page.locator("#import-result")).to_contain_text('"validated_records": 2')
    page.get_by_role("button", name="Commit previewed records", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#import-result")).to_contain_text("2/2 operations committed")
    page.get_by_role("button", name="First page / refresh", exact=True).click()
    expect(page.locator("#vectors tbody tr")).to_have_count(4)
    for label in ["Export current page JSON", "Export current page CSV"]:
        with page.expect_download() as event:
            page.get_by_role("button", name=label, exact=True).click()
        assert event.value.suggested_filename.startswith("vlite-page.")
    page.locator("#vectors").get_by_role("button", name="101", exact=True).click()
    expect(page.locator("#id")).to_have_value("101")
    expect(page.locator("#payload-preview")).to_contain_text("empty payload")
    page.get_by_role("button", name="Inspect 32 components", exact=True).click()
    expect(page.locator("#vector-slice-result")).to_contain_text('"end_exclusive": 3')
    page.get_by_role("button", name="Find similar to saved ID", exact=True).click()
    expect(page.locator("#search-summary")).to_contain_text("neighbors returned")
    page.get_by_text("Build an exact metadata filter", exact=True).click()
    page.locator("#filter-field").fill("category")
    page.locator("#filter-value").fill("demo")
    page.get_by_role("button", name="Add AND condition", exact=True).click()
    assert json.loads(page.locator("#filter").input_value())[0]["value"]["value"] == "demo"
    page.get_by_role("button", name="Search", exact=True).click()
    expect(page.locator("#hits tbody tr")).to_have_count(1)
    page.get_by_role("button", name="Clear filter", exact=True).click()
    page.locator("#compare-id").fill("102")
    page.get_by_role("button", name="Compare with editor ID", exact=True).click()
    expect(page.locator("#record-comparison")).to_contain_text('"squared_l2": 2')
    page.get_by_role("button", name="Analyze first 32 records", exact=True).click()
    expect(page.locator("#sample-analysis")).to_contain_text('"sampled_records": 4')
    expect(page.locator("#sample-analysis")).to_contain_text('"mixed_types": true')
    expect(page.locator("#projection svg circle")).to_have_count(4)
    page.get_by_role("button", name="Build HNSW", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#build-result")).to_contain_text("Build completed")
    page.get_by_role("button", name="Compare exact / ready ANN", exact=True).click()
    expect(page.locator("#search-comparison")).to_contain_text('"overlap_fraction": 1')
    page.locator("#scenario").fill(json.dumps({
        "name": "invalid-credential-field",
        "steps": [{"path": "/v1/vectors/101", "expected_ids": ["101"], "headers": {"Authorization": "not-allowed"}}],
    }))
    page.get_by_role("button", name="Run scenario", exact=True).click()
    expect(page.locator("#status")).to_contain_text("credentials/headers are not allowed")
    page.locator("#scenario").fill(json.dumps({
        "name": "get-demo",
        "steps": [{"path": "/v1/vectors/101", "expected_ids": ["101"]}],
    }))
    page.get_by_role("button", name="Run scenario", exact=True).click()
    expect(page.locator("#scenario-result")).to_contain_text('"passed": true')
    with page.expect_download():
        page.get_by_role("button", name="Download scenario", exact=True).click()
    page.get_by_role("button", name="Generate redacted examples", exact=True).click()
    expect(page.locator("#code-examples")).to_contain_text("VLITE_TOKEN")
    expect(page.locator("#code-examples")).not_to_contain_text(token)
    page.get_by_role("button", name="Send request", exact=True).click()
    expect(page.locator("#inspector")).to_contain_text('"records": 4')
    page.get_by_role("button", name="Refresh health snapshot", exact=True).click()
    expect(page.locator("#health-result")).to_contain_text('"observed_at"')
    page.get_by_role("button", name="First page / refresh", exact=True).click()
    expect(page.locator("#vectors tbody tr")).to_have_count(4)
    page.locator("#batch-ids").fill("101,102")
    page.locator("#batch-metadata").fill('{"tag":{"type":"string","value":"updated"}}')
    page.get_by_role("button", name="Replace selected metadata", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#import-result")).to_contain_text("2/2 operations committed")
    page.get_by_role("button", name="First page / refresh", exact=True).click()
    expect(page.locator("#vectors tbody")).to_contain_text("updated")
    page.get_by_role("button", name="Delete selected", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#import-result")).to_contain_text("2/2 operations committed")
    held = []
    page.route("**/api/v1/vectors/999", lambda route: held.append(route))
    page.locator("#id").fill("999")
    page.get_by_role("button", name="Get", exact=True).click()
    expect(page.locator("#cancel-request")).to_be_enabled()
    page.get_by_role("button", name="Stop waiting", exact=True).click()
    expect(page.locator("#status")).to_contain_text("mutation outcome may be unknown")
    expect(page.locator("#error-help")).to_contain_text("does not cancel server work")
    assert len(held) == 1
    held[0].abort()
    page.unroute("**/api/v1/vectors/999")
    page.get_by_role("button", name="Connect / refresh", exact=True).click()
    expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
    page.get_by_role("button", name="First page / refresh", exact=True).click()
    expect(page.locator("#vectors tbody tr")).to_have_count(2)
    page.route("**/api/v1/vectors/999", lambda route: route.fulfill(
        status=500, content_type="application/json",
        body='{"error":{"code":"internal","message":"Write outcome unknown"}}'))
    page.locator("#id").fill("999")
    page.get_by_role("button", name="Put / replace", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#status")).to_contain_text("HTTP 500")
    expect(page.locator("#stat-records")).to_have_text("--")
    expect(page.locator("#vectors tbody tr")).to_have_count(0)
    page.unroute("**/api/v1/vectors/999")
    page.get_by_role("button", name="Disconnect", exact=True).click()
    expect(page.locator("#sample-analysis")).to_have_text("")
    expect(page.locator("#code-examples")).to_have_text("")
    page.set_viewport_size({"width": 390, "height": 844})
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")


def exercise_operations(page, base, token, read_token):
    page.goto(base + "/")
    page.locator("#token").fill(token)
    page.get_by_role("button", name="Connect / refresh").click()
    expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
    page.get_by_role("button", name="Inspect credential sources", exact=True).click()
    expect(page.locator("#token-result")).to_contain_text('"read_only_configured": true')
    page.get_by_role("button", name="Read sanitized audit trail", exact=True).click()
    expect(page.locator("#audit-result")).to_contain_text('"capacity": 256')
    expect(page.locator("#audit-result")).not_to_contain_text(token)
    page.get_by_text("HNSW configuration and background maintenance", exact=True).click()
    page.get_by_role("button", name="Read maintenance policy", exact=True).click()
    expect(page.locator("#maintenance-result")).to_contain_text('"pressure"')
    page.locator("#hnsw-m").fill("8")
    page.locator("#hnsw-ef").fill("40")
    page.get_by_role("button", name="Start background rebuild", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#rebuild-result")).to_contain_text('"outcome": "running"')
    for _ in range(100):
        page.get_by_role("button", name="Poll / publish rebuild", exact=True).click()
        expect(page.locator("#rebuild-result")).to_contain_text('"outcome"')
        if '"published"' in page.locator("#rebuild-result").inner_text():
            break
        time.sleep(.05)
    expect(page.locator("#rebuild-result")).to_contain_text('"published"')
    page.locator("#query").fill("[1,0,0]")
    page.locator("#filter").fill("[]")
    page.locator("#mode").select_option("approximate_ready")
    page.get_by_role("button", name="Search", exact=True).click()
    expect(page.locator("#hits tbody tr")).to_have_count(2)
    page.get_by_role("button", name="Replace maintenance policy", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#maintenance-result")).to_contain_text('"policy"')
    page.get_by_role("button", name="Create consistent backup", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#backup-result")).to_contain_text('"inspection"')
    expect(page.locator("#backup-name option")).to_have_count(1)
    page.get_by_role("button", name="Validate / stage restored copy", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#restore-name option")).to_have_count(1)
    page.get_by_role("button", name="Select restored copy for restart", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#backup-result")).to_contain_text('"restart_required": true')
    page.get_by_role("button", name="Disconnect", exact=True).click()
    page.locator("#token").fill(read_token)
    page.get_by_role("button", name="Connect / refresh").click()
    expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
    expect(page.locator("#compatibility")).to_contain_text('"role": "read_only"')
    expect(page.locator("#delete-vector")).to_be_disabled()
    expect(page.locator("#import-commit")).to_be_disabled()
    expect(page.locator("#create-backup")).to_be_disabled()
    page.locator("#mode").select_option("exact")
    page.get_by_role("button", name="Search", exact=True).click()
    expect(page.locator("#hits tbody tr")).to_have_count(2)
    # Direct API access still enforces the role independently of the console.
    headers = {"Authorization": "Bearer " + read_token}
    assert page.request.put(base + "/api/v1/vectors/500", headers=headers, data={"vector": [1, 0, 0]}).status == 403
    assert page.request.get(base + "/api/v1/admin/audit", headers=headers).status == 403
    assert page.request.post(base + "/api/v1/search", headers=headers, data={"vector": [1, 0, 0], "k": 1, "mode": "approximate"}).status == 403
    page.get_by_role("button", name="Disconnect", exact=True).click()
    page.locator("#token").fill(token)
    page.get_by_role("button", name="Connect / refresh").click()
    expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
    page.get_by_role("button", name="Revoke read-only credential", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#token-result")).to_contain_text('"read_only_configured": false')
    assert page.request.get(base + "/api/v1/database", headers=headers).status == 401
    page.get_by_role("button", name="Reload configured token files", exact=True).click()
    page.get_by_role("button", name="Confirm", exact=True).click()
    expect(page.locator("#status")).to_contain_text("Configured credentials reloaded")
    expect(page.locator("#connection-badge")).to_have_text("Disconnected")
    assert page.request.get(base + "/api/v1/database", headers=headers).status == 200


def main():
    name = "vlite-dashboard-" + uuid.uuid4().hex[:10]
    volume = name + "-data"
    network = name + "-network"
    ui_name = name + "-ui"
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
                    page.set_default_timeout(15000)
                    base = "http://127.0.0.1:" + port
                    ready(page, base)
                    exercise(page, base, token)
                    ui_base = "http://127.0.0.1:" + ui_port
                    ready(page, ui_base + "/api")
                    exercise_separate_app(page, ui_base, token)
                    docker("restart", name)
                    # Docker may reassign an ephemeral host port across restart.
                    port = docker("port", name, "8080/tcp").rsplit(":", 1)[1]
                    base = "http://127.0.0.1:" + port
                    ready(page, base)
                    page.goto(base + "/dashboard")
                    page.locator("#token").fill(token)
                    page.get_by_role("button", name="Connect / refresh").click()
                    expect(page.locator("#status")).to_have_text("Connected; overview refreshed")
                    expect(page.locator("#index-status")).to_contain_text("Not ready")
                    page.locator("#id").fill("2")
                    page.get_by_role("button", name="Get", exact=True).click()
                    expect(page.locator("#record")).to_contain_text('"payload_base64": "AP8="')
                    page.get_by_role("button", name="Disconnect", exact=True).click()
                    # Fresh page has no bearer credential; every API still fails closed.
                    assert page.request.get(base + "/v1/database").status == 401
                    ready(page, ui_base + "/api")
                    exercise_separate_app(page, ui_base, token, "Not ready")
                    exercise_console_tools(page, ui_base, token)
                    exercise_operations(page, ui_base, token, read_token)
                    print("PASS Chromium dashboards: embedded REST UI and separately containerized REST-proxy UI; auth, CRUD, pagination, search, index readiness, confirmation, token lifecycle and API restart persistence")
                finally:
                    browser.close()
    finally:
        subprocess.run(["docker", "rm", "-f", ui_name, name], check=False, capture_output=True, timeout=60)
        subprocess.run(["docker", "network", "rm", network], check=False, capture_output=True, timeout=60)
        print("Retained Docker data volume:", volume)


if __name__ == "__main__":
    main()
