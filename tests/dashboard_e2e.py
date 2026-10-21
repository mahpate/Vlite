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
    return subprocess.run(["docker", *args], check=True, capture_output=True, text=True, timeout=600, **kwargs).stdout.strip()

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
    expect(page.locator("#index-status")).to_contain_text(expected_index)
    page.get_by_role("button", name="First page / refresh", exact=True).click()
    expect(page.locator("#vectors tbody tr")).to_have_count(2)
    page.locator("#query").fill("[1,0,0]")
    page.locator("#k").fill("2")
    page.get_by_role("button", name="Search", exact=True).click()
    expect(page.locator("#hits tbody tr")).to_have_count(2)
    expect(page.locator("#inspector")).to_contain_text('"distance": 0')
    assert any(url.endswith("/api/v1/database") for url in api_requests), api_requests
    assert any(url.endswith("/api/v1/search") for url in api_requests), api_requests
    assert page.evaluate("localStorage.length + sessionStorage.length") == 0
    page.get_by_role("button", name="Disconnect", exact=True).click()
    expect(page.locator("#overview")).to_have_text("")
    assert not errors, errors


def main():
    name = "vlite-dashboard-" + uuid.uuid4().hex[:10]
    volume = name + "-data"
    network = name + "-network"
    ui_name = name + "-ui"
    image = os.environ.get("VLITE_IMAGE", "vlite-dashboard:test")
    ui_image = os.environ.get("VLITE_DASHBOARD_IMAGE", "vlite-dashboard-ui:test")
    token = secrets.token_urlsafe(32)
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
            docker("run", "-d", "--name", name, "--network", network, "--network-alias", "api",
                   "--mount", f"type=volume,source={volume},target=/data",
                   "--mount", f"type=bind,source={secret},target=/run/token,readonly",
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
                    print("PASS Chromium dashboards: embedded REST UI and separately containerized REST-proxy UI; auth, CRUD, pagination, search, index readiness, confirmation, token lifecycle and API restart persistence")
                finally:
                    browser.close()
    finally:
        subprocess.run(["docker", "rm", "-f", ui_name, name], check=False, capture_output=True, timeout=60)
        subprocess.run(["docker", "network", "rm", network], check=False, capture_output=True, timeout=60)
        print("Retained Docker data volume:", volume)


if __name__ == "__main__":
    main()
