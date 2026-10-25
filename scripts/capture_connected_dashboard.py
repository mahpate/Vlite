"""Capture current-source dashboard workflows against an isolated real API.

Run on a disposable Linux Docker host with tests/requirements-dashboard.txt.
Only synthetic demo records are written; no existing server or volume is used.
"""

import base64
from functools import partial
import hashlib
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
from threading import Thread
import time
import urllib.error
import urllib.request
import uuid

from playwright.sync_api import expect, sync_playwright

ROOT = Path(__file__).resolve().parents[1]
IMAGE = os.environ.get("VLITE_CAPTURE_IMAGE", "ghcr.io/mahpate/vlite-api:v0.4.1")


def docker(*args, **kwargs):
    return subprocess.check_output(["docker", *args], text=True, **kwargs).strip()


def capture(output):
    output.mkdir(parents=True, exist_ok=True)
    token = secrets.token_urlsafe(32)
    name = "vlite-connected-tour-" + uuid.uuid4().hex[:10]
    env = {**os.environ, "VLITE_AUTH_TOKEN": token}
    docker("run", "-d", "--name", name,
           "--mount", "type=tmpfs,target=/data,tmpfs-mode=1777",
           "--mount", "type=tmpfs,target=/admin,tmpfs-mode=1777",
           "-e", "VLITE_AUTH_TOKEN", "-e", "VLITE_ADMIN_DIR=/admin",
           "-p", "127.0.0.1::8080", "--entrypoint", "vlite-server",
           IMAGE, "/data/demo.vlite", "--create-if-missing", "3", "--create-format", "5",
           "--bind", "0.0.0.0:8080", "--allow-remote", env=env)
    server = None
    try:
        port = docker("port", name, "8080/tcp").rsplit(":", 1)[1]
        backend = "http://127.0.0.1:" + port

        def api(path, body=None, method=None):
            request = urllib.request.Request(
                backend + path, data=json.dumps(body).encode() if body is not None else None,
                method=method, headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
            with urllib.request.urlopen(request, timeout=15) as response:
                if response.status == 204:
                    return None
                return json.load(response)

        deadline = time.monotonic() + 60
        while True:
            try:
                api("/health")
                break
            except (OSError, urllib.error.URLError):
                if time.monotonic() > deadline:
                    raise
                time.sleep(.5)
        capabilities = api("/v1/capabilities")
        required = ("recommendations", "facets", "change_feed", "tenants",
                    "scoped_credentials", "auto_maintenance", "backup_restore", "webhooks")
        unavailable = [feature for feature in required if not capabilities["features"].get(feature)]
        if unavailable:
            raise RuntimeError(f"Capture database lacks required capabilities: {unavailable}")
        titles = (
            "Getting started with VLite", "Vector search for support articles",
            "Hybrid search with typed metadata", "Choosing an HNSW index",
            "Keeping database backups", "Monitoring API health",
            "Connecting Python applications", "Managing scoped credentials",
        )
        for i, title in enumerate(titles, 101):
            api(f"/v1/vectors/{i}", {
                "vector": [round(1 - (i - 101) * .08, 2), round((i - 101) * .12, 2), .1],
                "payload_base64": base64.b64encode((title + "\nA sample article in the VLite knowledge base.").encode()).decode(),
                "metadata": {"title": {"type": "string", "value": title},
                             "category": {"type": "string", "value": "guides" if i < 105 else "operations"}},
            }, "PUT")
        settings = api("/v1/collections/default")["settings"]
        settings["text_fields"] = ["title"]
        api("/v1/collections/default/settings", settings, "PUT")
        api("/v1/collections", {"name": "support", "dimensions": 3}, "POST")
        api("/v1/collections", {"name": "products", "dimensions": 3}, "POST")
        api("/v1/index/hnsw", {}, "POST")
        api("/v1/admin/backups", {}, "POST")

        class Handler(SimpleHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def translate_path(self, path):
                if path == "/dashboard":
                    path = "/"
                elif path.startswith("/dashboard/"):
                    path = path[len("/dashboard"):]
                return super().translate_path(path)

            def proxy(self):
                path = self.path.removeprefix("/api")
                body = self.rfile.read(int(self.headers.get("Content-Length", 0))) or None
                request = urllib.request.Request(
                    backend + path, data=body, method=self.command,
                    headers={key: self.headers[key] for key in ("Authorization", "Content-Type") if key in self.headers})
                try:
                    response = urllib.request.urlopen(request, timeout=20)
                except urllib.error.HTTPError as error:
                    response = error
                with response:
                    data = response.read()
                    self.send_response(response.status)
                    for key in ("Content-Type", "ETag", "X-Request-Id"):
                        if response.headers.get(key):
                            self.send_header(key, response.headers[key])
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)

            def do_GET(self):
                if self.path.startswith(("/api/", "/v1/", "/health", "/ready", "/metrics")):
                    self.proxy()
                else:
                    super().do_GET()

            do_POST = proxy
            do_PUT = proxy
            do_DELETE = proxy

        server = ThreadingHTTPServer(("127.0.0.1", 0), partial(Handler, directory=str(ROOT / "src" / "dashboard")))
        Thread(target=server.serve_forever, daemon=True).start()
        captures = []
        with sync_playwright() as p:
            browser = p.chromium.launch()
            page = browser.new_page(viewport={"width": 1440, "height": 1100},
                                    device_scale_factor=1, reduced_motion="reduce")
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.goto(f"http://127.0.0.1:{server.server_port}/dashboard/")
            expect(page.locator("#tour-dismiss")).to_be_visible()
            page.locator("#tour-dismiss").click()
            page.locator("#token").fill(token)
            page.locator("#connection button[type=submit]").click()
            expect(page.locator("#connection-badge")).to_have_text("Connected")
            expect(page.locator("#stat-records")).to_have_text(str(len(titles)))
            nav = page.get_by_role("navigation", name="Dashboard navigation")

            def show(selector):
                locator = page.locator(selector)
                locator.evaluate("""element => {
                    const section = element.closest("main > section");
                    const panel = element.closest("[data-admin-view]");
                    if (panel && panel.id !== "admin-credential-form") window.VLiteNavigation.reveal(panel.id);
                    else if (section && section.hidden) window.VLiteNavigation.reveal(section.id);
                    for (let parent = element.parentElement; parent; parent = parent.parentElement) {
                        if (parent.tagName === "DETAILS") parent.open = true;
                    }
                }""")
                return locator

            def save(filename, focus=None):
                expect(page.locator("#connection-badge")).to_have_text("Connected")
                expect(page.locator('[data-loading="true"]')).to_have_count(0)
                if focus:
                    page.locator(focus).scroll_into_view_if_needed()
                else:
                    page.evaluate("window.scrollTo(0, 0)")
                # Never expose even a masked token length or credential through a screenshot.
                page.screenshot(path=str(output / filename), animations="disabled",
                                mask=[page.locator("#token")], mask_color="#e4e6ef")
                captures.append({"file": filename, "workspace": page.locator("#workspace-title").inner_text(),
                                 "sha256": hashlib.sha256((output / filename).read_bytes()).hexdigest()})

            preferences = page.locator(".topbar .appearance-settings")
            preferences.locator("summary").click()
            page.locator("#theme").select_option("light")
            preferences.locator("summary").click()
            save("current-overview-light.png")
            page.locator(".token-help summary").click()
            save("current-token-help.png")
            page.locator(".token-help summary").click()
            nav.get_by_role("link", name="Collections", exact=True).click()
            page.locator("#browse-form button[type=submit]").click()
            expect(page.locator("#vectors tbody tr")).to_have_count(len(titles))
            save("current-records.png")
            page.locator("#vectors tbody button").first.click()
            expect(page.locator("#record-inspector-payload")).to_contain_text("Getting started")
            save("current-payload.png", "#record-inspector")
            for tab in ("Metadata", "Vector"):
                page.get_by_role("tablist", name="Selected record views").get_by_role("tab", name=tab, exact=True).click()
                save(f"current-{tab.lower()}.png", "#record-inspector")
            page.get_by_role("tab", name="Write", exact=True).click()
            show("#write-documents").fill(json.dumps([{
                "id": "201", "vector": [.85, .15, .1],
                "metadata": {"title": {"type": "string", "value": "A new support article"}},
                "payload": base64.b64encode(b"Ready for validation, not committed.").decode(),
            }], indent=2))
            page.locator("#write-validate").click()
            expect(page.locator("#write-commit")).to_be_enabled()
            save("current-write.png")
            page.get_by_role("tab", name="Index", exact=True).click()
            show("#index-status")
            expect(page.locator("#index-status")).not_to_contain_text("not checked")
            save("current-index.png")
            page.get_by_role("tab", name="Settings", exact=True).click()
            show("#settings-load").click()
            expect(page.locator("#collection-settings")).to_have_value(re.compile("title"))
            save("current-collections.png")

            nav.get_by_role("link", name="Search", exact=True).click()
            page.locator("#query").fill("[0.9, 0.1, 0]")
            page.locator("#search-form button[type=submit]").click()
            expect(page.locator("#hits tbody tr")).to_have_count(len(titles))
            save("current-search.png", "#search-summary")
            page.locator("#mode").select_option("hybrid")
            show("#search-text").fill("vector search")
            page.locator("#search-form button[type=submit]").click()
            expect(page.locator("#search-summary")).to_contain_text("hybrid")
            expect(page.locator("#hits tbody tr")).not_to_have_count(0)
            save("current-hybrid.png", "#search-summary")
            show("#recommend-positive").fill("101")
            page.locator("#recommend-run").click()
            expect(page.locator("#recommend-result")).to_contain_text('"id"')
            page.locator("#facets-run").click()
            expect(page.locator("#facets-result")).to_contain_text("guides")
            save("current-retrieval.png", "#recommend-result")
            show("#analyze-sample").click()
            expect(page.locator("#projection svg")).to_be_visible()
            save("current-explore.png", "#projection")

            show("#import-file").set_input_files({
                "name": "sample-articles.json", "mimeType": "application/json",
                "buffer": json.dumps([{"id": "301", "vector": [.8, .2, .1],
                                       "metadata": {"category": {"type": "string", "value": "guides"}}}]).encode(),
            })
            page.locator("#import-preview").click()
            expect(page.locator("#import-commit")).to_be_enabled()
            save("current-import.png")

            nav.get_by_role("link", name="Observability", exact=True).click()
            page.locator("#obs-interval").select_option("0")
            page.locator("#obs-refresh").click()
            expect(page.locator("#obs-metrics-status")).to_contain_text("Live metrics")
            page.wait_for_timeout(1100)
            for _ in range(5):
                api("/v1/database")
            page.locator("#obs-refresh").click()
            expect(page.locator("#obs-routes tr")).not_to_have_count(0)
            expect(page.locator("#obs-rps")).not_to_have_text("\u2014")
            save("current-observability.png")
            show("#feed-load").click()
            expect(page.locator("#feed-result")).not_to_have_text("")
            save("current-feed.png")
            nav.get_by_role("link", name="Administration", exact=True).click()
            page.locator("#refresh-health").click()
            expect(page.locator("#health-result")).to_contain_text("observed_at")
            save("current-administration.png")
            show("#list-backups").click()
            expect(page.locator("#backup-name option")).to_have_count(1)
            save("current-backups.png")
            show("#cred-list").click()
            expect(page.locator("#cred-result")).to_contain_text("credential slots used")
            save("current-credentials.png", "#cred-create")
            show("#tenant-name").fill("Demo workspace")
            page.locator("#tenant-max-vectors").fill("1000")
            page.locator("#tenant-create").click()
            expect(page.locator("#confirmation-text")).to_contain_text('Create tenant "Demo workspace"')
            page.locator("#confirmation button[value=confirm]").click()
            expect(page.locator("#tenant-result")).to_contain_text("created")
            page.locator("#tenant-list").click()
            expect(page.locator("#tenant-result")).to_contain_text("Demo workspace")
            save("current-tenants.png")
            show("#auto-maintenance-read").click()
            expect(page.locator("#auto-maintenance-result")).to_contain_text("config")
            save("current-automation.png")
            show("#webhook-list").click()
            expect(page.locator("#webhook-result")).to_contain_text("items")
            save("current-webhooks.png", "#webhook-result")
            show("#tls-result")
            expect(page.locator("#tls-result")).to_contain_text("file-backed TLS")
            save("current-tls.png")
            show("#replication-max-bytes").fill("1048576")
            page.locator("#replication-export").click()
            expect(page.locator("#replication-download")).to_be_enabled()
            expect(page.locator("#replication-result")).to_contain_text("head_sequence")
            save("current-replication.png")
            nav.get_by_role("link", name="Developer Console", exact=True).click()
            page.locator("#send-request").click()
            expect(page.locator("#inspector")).to_contain_text("/v1/database")
            save("current-developer.png")
            nav.get_by_role("link", name="Overview", exact=True).click()
            preferences.locator("summary").click()
            page.locator("#theme").select_option("dark")
            preferences.locator("summary").click()
            save("current-overview-dark.png")
            preferences.locator("summary").click()
            save("current-preferences.png")
            assert not errors, errors
            assert token not in page.locator("body").inner_text()
            browser.close()
        assert len({item["sha256"] for item in captures}) == len(captures)
        manifest = {
            "data": "Synthetic demo records in an isolated disposable database",
            "record_count": len(titles), "connected": True, "backend_image": IMAGE,
            "format_version": capabilities["format_version"],
            "source_assets": {file.name: hashlib.sha256(file.read_bytes()).hexdigest()
                              for file in (ROOT / "src" / "dashboard").iterdir() if file.is_file()},
            "captures": captures,
        }
        (output / "connected-captures.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"Captured {len(captures)} distinct connected views across all six workspaces.")
    finally:
        if server:
            server.shutdown()
            server.server_close()
        docker("rm", "-f", name)


if __name__ == "__main__":
    capture(Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / "site" / "assets" / "dashboard")
