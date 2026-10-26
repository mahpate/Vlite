"""Capture current-source dashboard workflows against isolated real APIs.

Run on a disposable Linux Docker host with tests/requirements-dashboard.txt.
Only synthetic demo records are written; no existing server or volume is used.
The Graph view comes from a second, graph-enabled database with the same records,
because graph-enabled files have no change feed or webhooks.
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
IMAGE = os.environ.get("VLITE_CAPTURE_IMAGE", "ghcr.io/mahpate/vlite-api:v0.5.0")
ARTICLES = (
    ("getting-started", "Getting started with VLite"), ("vector-search", "Vector search for support articles"),
    ("hybrid-search", "Hybrid search with typed metadata"), ("hnsw-index", "Choosing an HNSW index"),
    ("backups", "Keeping database backups"), ("monitoring", "Monitoring API health"),
    ("python-client", "Connecting Python applications"), ("credentials", "Managing scoped credentials"),
)
# Reading order between the articles; a traversal from getting-started reaches all of them in two hops.
LINKS = (
    ("getting-started", "NEXT", "vector-search"), ("getting-started", "NEXT", "python-client"),
    ("getting-started", "NEXT", "backups"), ("vector-search", "NEXT", "hybrid-search"),
    ("vector-search", "NEXT", "hnsw-index"), ("python-client", "NEXT", "credentials"),
    ("backups", "SEE_ALSO", "monitoring"),
)
# vlite-server creates only plain files: a private server creates an empty one, then
# vlite-maintain copies the closed file into a graph-enabled one for the real server.
GRAPH_BOOTSTRAP = """set -eu
env -u VLITE_ADMIN_DIR -u VLITE_TRUSTED_PROXY vlite-server /data/seed.vlite --create-if-missing 3 --create-format 5 --bind 127.0.0.1:8081 >/data/seed.log 2>&1 &
seed=$!
tries=0
until grep -q '^LISTENING ' /data/seed.log; do
  tries=$((tries + 1))
  if [ "$tries" -gt 300 ] || ! kill -0 "$seed" 2>/dev/null; then cat /data/seed.log >&2; exit 1; fi
  sleep 0.2
done
kill -TERM "$seed"
if ! wait "$seed"; then cat /data/seed.log >&2; exit 1; fi
vlite-maintain enable-graph /data/seed.vlite /data/demo.vlite
rm -f /data/seed.*
exec vlite-server /data/demo.vlite --bind 0.0.0.0:8080 --allow-remote
"""


def docker(*args, **kwargs):
    return subprocess.check_output(["docker", *args], text=True, **kwargs).strip()


def start(name, token, graph=False):
    """Run a disposable server on empty tmpfs volumes."""
    if graph:
        command = ("--entrypoint", "sh", IMAGE, "-c", GRAPH_BOOTSTRAP)
    else:
        command = ("--entrypoint", "vlite-server", IMAGE, "/data/demo.vlite", "--create-if-missing", "3",
                   "--create-format", "5", "--bind", "0.0.0.0:8080", "--allow-remote")
    # VLITE_TRUSTED_PROXY lets the sign-in screen offer passwords, as it does behind an HTTPS proxy.
    docker("run", "-d", "--name", name,
           "--mount", "type=tmpfs,target=/data,tmpfs-mode=1777",
           "--mount", "type=tmpfs,target=/admin,tmpfs-mode=1777",
           "-e", "VLITE_AUTH_TOKEN", "-e", "VLITE_ADMIN_DIR=/admin", "-e", "VLITE_TRUSTED_PROXY=1",
           "-p", "127.0.0.1::8080", *command, env={**os.environ, "VLITE_AUTH_TOKEN": token})


def connect(name, token):
    """Wait for the server's health route; return its address and an authenticated JSON helper."""
    backend = "http://127.0.0.1:" + docker("port", name, "8080/tcp").rsplit(":", 1)[1]

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
            return backend, api
        except (OSError, urllib.error.URLError):
            if time.monotonic() > deadline:
                subprocess.run(["docker", "logs", name], check=False)
                raise
            time.sleep(.5)


def seed(api):
    """Write the synthetic articles and the extra collections that both databases show."""
    for i, (_, title) in enumerate(ARTICLES, 101):
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


def seed_graph(api):
    """Link the articles in an `articles` graph whose nodes are attached to their records."""
    api("/v1/graphs", {"graph": "articles"}, "POST")
    ops = []
    for i, (key, title) in enumerate(ARTICLES, 101):
        ops.append({"op": "create_node", "key": key, "labels": ["Guide" if i < 105 else "Operations"],
                    "properties": {"title": {"type": "string", "value": title}}})
        ops.append({"op": "attach", "node": key, "collection": "default", "record": str(i)})
    ops += [{"op": "create_edge", "source": source, "relationship": relationship, "destination": destination}
            for source, relationship, destination in LINKS]
    api("/v1/graphs/articles/batch", {"ops": ops}, "POST")
    if [graph["name"] for graph in api("/v1/graphs")["graphs"]] != ["articles"]:
        raise RuntimeError("Capture database does not list the articles graph")


def dashboard_server(backend):
    """Serve the current dashboard source and proxy API routes to one backend."""

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
    return server


def capture(output):
    output.mkdir(parents=True, exist_ok=True)
    token = secrets.token_urlsafe(32)
    names = ["vlite-connected-tour-" + uuid.uuid4().hex[:10] for _ in range(2)]
    servers = []
    try:
        start(names[0], token)
        # Graph-enabled files reject the change feed and webhooks, so the Graph view comes from a
        # second disposable database with the same articles and a small graph linking them.
        start(names[1], token, graph=True)
        backend, api = connect(names[0], token)
        graph_backend, graph_api = connect(names[1], token)
        capabilities = api("/v1/capabilities")
        required = ("recommendations", "facets", "change_feed", "tenants",
                    "scoped_credentials", "auto_maintenance", "backup_restore", "webhooks")
        unavailable = [feature for feature in required if not capabilities["features"].get(feature)]
        if unavailable:
            raise RuntimeError(f"Capture database lacks required capabilities: {unavailable}")
        seed(api)
        api("/v1/index/hnsw", {}, "POST")
        api("/v1/admin/backups", {}, "POST")
        seed(graph_api)
        seed_graph(graph_api)
        # Sample accounts with random, unsaved passwords; the browser signs in with the token.
        accounts = (("maya", "super_admin"), ("alex", "editor"), ("sam", "viewer"))
        for call in (api, graph_api):
            for username, role in accounts:
                call("/v1/admin/users", {"username": username, "password": secrets.token_urlsafe(24), "role": role}, "POST")
            if not call("/v1/auth/config")["password_login"]:
                raise RuntimeError("Capture server does not offer password sign-in")
        servers += [dashboard_server(backend), dashboard_server(graph_backend)]
        captures = []
        with sync_playwright() as p:
            browser = p.chromium.launch()
            page = browser.new_page(viewport={"width": 1440, "height": 1100}, color_scheme="light",
                                    device_scale_factor=1, reduced_motion="reduce")
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            page.goto(f"http://127.0.0.1:{servers[0].server_port}/dashboard/")
            nav = page.get_by_role("navigation", name="Dashboard navigation")
            menu = page.locator("#user-menu > summary")

            def show(selector):
                locator = page.locator(selector)
                locator.evaluate("""element => {
                    const section = element.closest("main > section");
                    if (section && section.hidden) window.VLiteNavigation.reveal(section.id);
                    for (let parent = element.parentElement; parent; parent = parent.parentElement) {
                        if (parent.tagName === "DETAILS") parent.open = true;
                    }
                }""")
                return locator

            def save(filename, focus=None, workspace=None):
                if workspace is None:
                    expect(page.locator("#connection-badge")).to_have_text("Connected")
                expect(page.locator('[data-loading="true"]')).to_have_count(0)
                if focus:
                    page.locator(focus).scroll_into_view_if_needed()
                else:
                    page.evaluate("window.scrollTo(0, 0)")
                # Never expose even a masked token length or credential through a screenshot.
                page.screenshot(path=str(output / filename), animations="disabled",
                                mask=[page.locator("#token:visible")], mask_color="#e4e6ef")
                captures.append({"file": filename,
                                 "workspace": workspace or page.locator("#workspace-title").inner_text(),
                                 "sha256": hashlib.sha256((output / filename).read_bytes()).hexdigest()})

            def theme(value):
                menu.click()
                page.locator("#theme").select_option(value)
                menu.click()

            def sign_in():
                page.locator("#token-sign-in > summary").click()
                page.locator("#token").fill(token)
                page.get_by_role("button", name="Sign in with token", exact=True).click()
                expect(page.locator("#connection-badge")).to_have_text("Connected")
                expect(page.locator("#stat-records")).to_have_text(str(len(ARTICLES)))
                page.locator("#tour-dismiss").click()
                theme("light")

            expect(page.locator("#sign-in")).to_be_visible()
            expect(page.locator("#app-shell")).to_be_hidden()
            expect(page.locator("#token")).to_be_hidden()
            save("current-sign-in.png", workspace="Sign in")
            sign_in()
            save("current-overview-light.png")
            nav.get_by_role("link", name="Collections", exact=True).click()
            page.locator("#browse-form button[type=submit]").click()
            expect(page.locator("#vectors tbody tr")).to_have_count(len(ARTICLES))
            save("current-records.png")
            page.locator("#vectors tbody button").first.click()
            expect(page.locator("#record-inspector-payload")).to_contain_text("Getting started")
            save("current-payload.png", "#record-inspector")
            for tab in ("Metadata", "Vector"):
                page.get_by_role("tablist", name="Selected record views").get_by_role("tab", name=tab, exact=True).click()
                save(f"current-{tab.lower()}.png", "#record-inspector")
            page.get_by_role("tab", name="Add & edit", exact=True).click()
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
            page.get_by_role("tab", name="Manage", exact=True).click()
            show("#settings-load").click()
            expect(page.locator("#collection-settings")).to_have_value(re.compile("title"))
            save("current-collections.png")

            nav.get_by_role("link", name="Search", exact=True).click()
            page.locator("#query").fill("[0.9, 0.1, 0]")
            page.locator("#search-form button[type=submit]").click()
            expect(page.locator("#hits tbody tr")).to_have_count(len(ARTICLES))
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
            show("#user-list").click()
            for username, _ in accounts:
                expect(page.locator("#user-result")).to_contain_text(username)
            save("current-users.png")
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
            theme("dark")
            save("current-overview-dark.png")
            menu.click()
            save("current-preferences.png")
            assert token not in page.locator("body").inner_text()
            # The graph database is another origin, so the dashboard starts signed out with fresh preferences.
            page.goto(f"http://127.0.0.1:{servers[1].server_port}/dashboard/")
            expect(page.locator("#sign-in")).to_be_visible()
            sign_in()
            nav.get_by_role("link", name="Graph", exact=True).click()
            expect(page.locator("#graph-controls")).to_be_visible()
            expect(page.locator("#graph-name")).to_have_value("articles")
            page.locator("#graph-operation").select_option("traverse")
            page.locator("#graph-start").fill(ARTICLES[0][0])
            page.locator("#graph-run").click()
            expect(page.locator("#graph-summary")).to_have_text(re.compile(r"^Complete"))
            expect(page.locator("#graph-view .graph-node")).to_have_count(len(ARTICLES))
            expect(page.locator("#graph-view .graph-edge")).to_have_count(len(LINKS))
            # The query form is taller than the window: frame its Run query row, just below the sticky
            # top bar, above the result and its node-link view.
            page.locator("#graph .graph-actions").evaluate("""element => window.scrollBy(0,
                element.getBoundingClientRect().top - document.querySelector(".topbar").getBoundingClientRect().bottom - 16)""")
            expect(page.locator("#graph-run")).to_be_in_viewport()
            expect(page.locator("#graph-view")).to_be_in_viewport(ratio=1)
            save("current-graph.png", "#graph-summary")
            assert not errors, errors
            assert token not in page.locator("body").inner_text()
            browser.close()
        assert len({item["sha256"] for item in captures}) == len(captures)
        manifest = {
            "data": "Synthetic demo records in isolated disposable databases",
            "graph_data": ("The Graph view uses a second, graph-enabled database with the same records, "
                           "because graph-enabled files have no change feed or webhooks"),
            "record_count": len(ARTICLES), "connected": True, "backend_image": IMAGE,
            "format_version": capabilities["format_version"],
            # Text-mode hashes match tests/dashboard_tours.py on any checkout line ending.
            "source_assets": {file.name: hashlib.sha256(file.read_text(encoding="utf-8").encode()).hexdigest()
                              for file in (ROOT / "src" / "dashboard").iterdir() if file.is_file()},
            "captures": captures,
        }
        (output / "connected-captures.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(f"Captured {len(captures)} distinct connected views: the sign-in screen and all seven workspaces.")
    finally:
        for server in servers:
            server.shutdown()
            server.server_close()
        subprocess.run(["docker", "rm", "-f", *names], check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    capture(Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT / "site" / "assets" / "dashboard")
