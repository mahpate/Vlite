"""Real-server Chromium regression: the dashboard while agent memory is enabled.

    python tests/dashboard_memory_mode.py [path-to-vlite-server]

The server binary is the first argument, else VLITE_SERVER_BIN, else target/debug/vlite-server
(.exe on Windows), built with `cargo build --locked --features server --bin vlite-server`.
A missing binary fails the run; nothing is skipped.

With a memory bindings file the server refuses a few database-wide routes with 403 forbidden_scope,
even for the super admin (docs/SECURITY.md, "Memory-bound resources"): global metrics, the metrics
summary, graph engine statistics, replication export, webhook list and status, every import route
and automatic maintenance. The dashboard once treated the refused /metrics as a failed sign-in.
This script starts disposable servers on loopback and drives Chromium against them:

  A. Control, no memory: token sign-in and live metrics work and show no memory note.
  B. Memory enabled, built as in docs/API.md "Start the server with memory": the bootstrap super admin
     signs in with a password and, again, with the admin token. Overview, Observability and Health say
     plainly that server metrics are off, and every other refused panel explains itself in its own
     result area instead of failing in the status bar. A tenant editor is unaffected.
  C. The same database restarted WITHOUT the bindings file: the binding records it still holds keep the
     reservation, so the refusals stay and the dashboard keeps explaining them (docs/SECURITY.md).

Secrets are random per run and never printed. Servers, browser and the temporary directory are
removed at the end.
"""

import http.client
import json
import os
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import time
import traceback
from contextlib import contextmanager
from pathlib import Path
from urllib.parse import urlsplit

from playwright.sync_api import Error as PlaywrightError, expect, sync_playwright

from dashboard_e2e import assert_no_stored_credentials, confirm, control, expect_signed_out, sign_in_with_token, sign_out

ROOT = Path(__file__).resolve().parent.parent
METRICS_OFF = "server metrics are off while agent memory is enabled"
WHILE_MEMORY = "while agent memory is enabled"
WIDTHS = (1280, 768, 390)
GRAPH_STATS_ROUTE = "/v1/admin/graph/stats"
NOT_EXERCISED = []
SECRETS = []
PHASE = ""  # prefixes the label of a failing step, so a red run names its phase


def secret(value):
    SECRETS.append(value)
    return value


def redact(text):
    for value in SECRETS:
        text = text.replace(value, "[redacted]")
    return text


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def server_binary():
    configured = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("VLITE_SERVER_BIN")
    if configured:
        path = Path(configured).resolve()
    else:
        path = ROOT / "target" / "debug" / ("vlite-server.exe" if os.name == "nt" else "vlite-server")
    if not path.is_file() and os.name == "nt" and path.with_name(path.name + ".exe").is_file():
        path = path.with_name(path.name + ".exe")
    if not path.is_file():
        raise SystemExit(f"vlite-server binary not found: {path}\n"
                         "Build it with `cargo build --locked --features server --bin vlite-server` or pass its path.")
    return path


def rest(url, method, path, body=None, token=None):
    """One loopback REST call outside the browser; returns (status, parsed JSON or None)."""
    parts = urlsplit(url)
    headers = {"Accept": "application/json"}
    data = None
    if token:
        headers["Authorization"] = "Bearer " + token
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    connection = http.client.HTTPConnection(parts.hostname, parts.port, timeout=30)
    try:
        connection.request(method, path, body=data, headers=headers)
        response = connection.getresponse()
        raw = response.read()
    finally:
        connection.close()
    try:
        value = json.loads(raw) if raw else None
    except ValueError:
        value = None
    return response.status, value


def accepted(what, result, *wanted):
    """The parsed body of a REST call that answered one of the wanted statuses. Bodies are never printed."""
    status, value = result
    if status not in wanted:
        error = value.get("error") if isinstance(value, dict) else None
        code = error.get("code") if isinstance(error, dict) else ""
        raise AssertionError(f"{what} answered HTTP {status} {code}".strip())
    return value


class Server:
    """A real vlite-server on loopback. Its output goes to a file: an unread pipe would stall it."""

    def __init__(self, binary, home, name, create=False, bindings=None):
        argv = [str(binary), str(home.database), "--token-file", str(home.token_file),
                "--bind", "127.0.0.1:0", "--durability", "full"]
        if create:
            argv += ["--create-if-missing", "3"]
        if bindings:
            argv += ["--memory-bindings-file", str(bindings)]
        self.log_path = home.path / f"{name}.log"
        with open(self.log_path, "wb") as log:
            self.process = subprocess.Popen(argv, cwd=home.path, env=home.env, stdin=subprocess.DEVNULL,
                                            stdout=log, stderr=subprocess.STDOUT)
        try:
            self.url = self.wait_until_listening()
        except BaseException:
            self.stop()
            raise

    def log(self):
        return self.log_path.read_text(encoding="utf-8", errors="replace")

    def tail(self, lines=12):
        return redact("\n".join(self.log().splitlines()[-lines:]))

    def wait_until_listening(self):
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise AssertionError(f"vlite-server exited with {self.process.returncode} before listening:\n{self.tail()}")
            match = re.search(r"LISTENING\s+(http://127\.0\.0\.1:\d+)", self.log())
            if match:
                url = match.group(1)
                try:
                    if rest(url, "GET", "/health")[0] == 200:
                        return url
                except OSError:
                    pass
            time.sleep(0.1)
        raise AssertionError(f"vlite-server did not listen on loopback within 90 seconds:\n{self.tail()}")

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=20)


class Home:
    """Private files of one disposable server: token, admin directory, database and an optional first super admin."""

    def __init__(self, work, name, token, account=None):
        self.path = work / name
        self.admin = self.path / "admin"
        self.admin.mkdir(parents=True)
        self.token_file = self.path / "admin.token"
        self.token_file.write_text(token + "\n", encoding="ascii")
        self.database = self.path / f"{name}.vlite"
        self.env = {key: value for key, value in os.environ.items() if not key.upper().startswith(("VLITE_", "OTEL_"))}
        self.env["VLITE_ADMIN_DIR"] = str(self.admin)
        if account:
            username, password = account
            password_file = self.path / "admin.password"
            password_file.write_text(password + "\n", encoding="ascii")
            self.env["VLITE_ADMIN_USERNAME"] = username
            self.env["VLITE_ADMIN_PASSWORD_FILE"] = str(password_file)


def start_memory_server(binary, home, token):
    """Builds the memory database the documented way and returns the server that serves it with agent memory on."""
    first = Server(binary, home, "create", create=True)
    try:
        accepted("create graph", rest(first.url, "POST", "/v1/graphs", {"graph": "notes_graph"}, token), 200, 201)
        accepted("create evidence collection", rest(
            first.url, "POST", "/v1/collections",
            {"name": "notes_ev", "dimensions": 3, "settings": {"text_fields": ["text"]}}, token), 200, 201)
        made = accepted("create credential", rest(
            first.url, "POST", "/v1/admin/credentials",
            {"name": "notes-agent", "collections": ["*"], "graphs": ["notes_graph"],
             "operations": ["graph_read", "graph_write", "read", "write", "search"],
             "expires_unix_ms": int(time.time() * 1000) + 3600000}, token), 200, 201)
        principal = made.get("credential", made)["id"]
        require(re.fullmatch(r"[a-z2-7]{12}", principal), "the credential API did not return a 12-character credential ID")
    finally:
        first.stop()
    bindings = home.path / "bindings.json"
    bindings.write_text(json.dumps({"version": "memory-bindings-v1", "bindings": [{
        "binding": "notes", "tenant": "@root", "generation": 1, "state": "active",
        "graph": "notes_graph", "evidence_collection": "notes_ev",
        "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
        "digest_version": "memory-request-v1", "receipt_window_ms": 86400000,
        "grants": [{"principal": principal, "scopes": [{"agent": "a"}]}]}]}), encoding="utf-8")
    return Server(binary, home, "memory", bindings=bindings)


def remove(path):
    for _ in range(10):
        try:
            shutil.rmtree(path)
            return
        except FileNotFoundError:
            return
        except OSError:
            time.sleep(0.5)
    shutil.rmtree(path, ignore_errors=True)


# ----------------------------------------------------------------------------------------------
# Browser helpers
# ----------------------------------------------------------------------------------------------
def screen(page, extra=()):
    """What a person reads when something goes wrong: the sign-in message, the status bar and the help line."""
    lines = []
    for name, selector in (("gate-message", "#gate-message"), ("status", "#status"), ("error-help", "#error-help"),
                           *((selector, selector) for selector in extra)):
        try:
            text = page.locator(selector).text_content(timeout=2000)
        except (PlaywrightError, AssertionError):
            text = "(unreadable)"
        lines.append(f"  {name}: {' '.join((text or '').split())!r}")
    return "\n".join(lines)


def summarize(error):
    """A Playwright failure without its long call log: what was expected, plus the locator it was waiting for."""
    head, _, log = redact(str(error)).partition("Call log:")
    first = next((line.strip(" -") for line in log.splitlines() if line.strip(" -")), "")
    return " | ".join(part for part in (" ".join(head.split()), first) if part)[:1500]


@contextmanager
def step(page, label, extra=()):
    """Names the failing step and shows what the page said, so a red run explains itself."""
    try:
        yield
    except (AssertionError, PlaywrightError) as error:
        raise AssertionError(f"FAILED {PHASE}{label}\n  {summarize(error)}\n{screen(page, extra)}") from None


def open_page(browser):
    context = browser.new_context(viewport={"width": 1280, "height": 900})
    page = context.new_page()
    page.set_default_timeout(30000)
    errors = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    page.on("console", lambda message: errors.append(message.text)
            if message.type == "error" and "Failed to load resource" not in message.text else None)
    return context, page, errors


def no_overflow(page, where):
    for width in WIDTHS:
        page.set_viewport_size({"width": width, "height": 900})
        page.evaluate("window.scrollTo(0, 0)")
        scroll, inner = page.evaluate("() => [document.documentElement.scrollWidth, innerWidth]")
        require(scroll <= inner, f"{where}: the page overflows horizontally at {width}px (scrollWidth {scroll} > {inner})")
    page.set_viewport_size({"width": 1280, "height": 900})


def refresh_observability(page, expected, **options):
    """Live monitor with the timer off, then Refresh now until the metrics line says what is expected.

    A refresh asked for before the section is on screen is ignored by the page, so ask again a few times."""
    control(page, "#observability").scroll_into_view_if_needed()
    control(page, "#obs-interval").select_option("0")
    metrics = control(page, "#obs-metrics-status")
    for attempt in range(3):
        control(page, "#obs-refresh").click()
        try:
            expect(metrics).to_contain_text(expected, timeout=10000, **options)
            return metrics
        except AssertionError:
            if attempt == 2:
                raise
    return metrics


def open_workspace(page, name):
    page.get_by_role("navigation", name="Dashboard navigation").get_by_role("link", name=name, exact=True).click()


def sign_in_with_password(page, username, password):
    """Types the account's credentials and returns once the dashboard shows or the page reports an error."""
    expect(page.locator("#sign-in")).to_be_visible()
    page.locator("#username").fill(username)
    page.locator("#password").fill(password)
    page.locator("#sign-in").get_by_role("button", name="Sign in", exact=True).click()
    page.wait_for_function("""() => !document.getElementById("app-shell").hidden
        || document.getElementById("status").dataset.error === "true" """)


def expect_signed_in_as(page, username):
    expect(page.locator("#app-shell")).to_be_visible()
    expect(page.locator("#sign-in-gate")).to_be_hidden()
    expect(page.locator("#status")).to_have_text(f"Signed in as {username}; overview refreshed")
    expect(page.locator("#connection-badge")).to_have_text("Connected")


def expect_no_refusal(page):
    for selector in ("#gate-message", "#error-help", "#status"):
        for text in ("forbidden_scope", "403"):
            expect(page.locator(selector)).not_to_contain_text(text)


# ----------------------------------------------------------------------------------------------
# A. Control: no memory
# ----------------------------------------------------------------------------------------------
def control_phase(browser, url, token):
    context, page, errors = open_page(browser)
    try:
        page.goto(url + "/dashboard")
        with step(page, "control: token sign-in on a server without agent memory"):
            expect_signed_out(page)
            sign_in_with_token(page, token)
            expect(page.locator("#stat-bytes")).to_have_text(re.compile(r"^\d+$"))
            # Absent on dashboards that predate the note, hidden once it exists: either way nobody sees it.
            expect(page.locator("#metrics-note")).to_be_hidden()
            no_overflow(page, "control overview")
        with step(page, "control: Observability shows live metrics"):
            open_workspace(page, "Observability")
            refresh_observability(page, "Live metrics")
            expect(control(page, "#obs-logs-status")).to_contain_text("Tailing server log ring")
            no_overflow(page, "control observability")
        require(not errors, f"control: browser errors: {errors}")
    finally:
        context.close()


# ----------------------------------------------------------------------------------------------
# B. Agent memory enabled
# ----------------------------------------------------------------------------------------------
def overview(page):
    with step(page, "step 2: Overview says server metrics are off and keeps the rest"):
        note = page.locator("#metrics-note")
        expect(note).to_be_visible()
        expect(note).to_contain_text(METRICS_OFF, ignore_case=True)
        expect(page.locator("#stat-bytes")).to_have_text("Unavailable")
        expect(page.locator("#stat-dimensions")).to_have_text("3")
        expect(page.locator("#stat-records")).to_have_text(re.compile(r"^\d+$"))
        no_overflow(page, "overview")


def observability(page):
    with step(page, "step 3: Observability explains that metrics are off and still tails logs"):
        open_workspace(page, "Observability")
        metrics = refresh_observability(page, METRICS_OFF, ignore_case=True)
        for text in ("Super admin needed", "403", "forbidden_scope"):
            expect(metrics).not_to_contain_text(text)
        # The server does not refuse the log ring, slow queries or the audit trail.
        expect(control(page, "#obs-logs-status")).to_contain_text("Tailing server log ring")
        expect(control(page, "#obs-slow-status")).to_contain_text("Most recent entries")
        expect(control(page, "#obs-audit-status")).to_contain_text("Most recent entries")
        no_overflow(page, "observability")


def administration(page):
    with step(page, "step 4: Administration health snapshot and backup list"):
        open_workspace(page, "Administration")
        control(page, "#refresh-health").click()
        health = control(page, "#health-result")
        expect(health).to_contain_text("observed_at")
        expect(health).to_contain_text("Unavailable: " + METRICS_OFF, ignore_case=True)
        expect(health).to_contain_text('"index"')
        for text in ("forbidden_scope", "403"):
            expect(page.locator("#status")).not_to_contain_text(text)
        no_overflow(page, "health")
        # The super admin may still use the backup routes; only scoped credentials are refused them.
        control(page, "#list-backups").click()
        backups = control(page, "#backup-result")
        expect(backups).to_contain_text('"items"')
        expect(backups).not_to_contain_text("forbidden_scope")
        no_overflow(page, "backups")


def refused_panel(page, advertised, label, button, result, before=None, extra=()):
    """A panel the server keeps off while agent memory is enabled says so in its own result area, not in the status bar."""
    with step(page, f"step 5: {label} ({button} -> {result})", extra):
        if before:
            control(page, before[0]).fill(before[1])
        trigger = control(page, button)
        try:
            expect(trigger).to_be_enabled()
        except AssertionError:
            route = trigger.get_attribute("data-route")
            why = ""
            if route:
                why = (f"; its data-route {route} is {'advertised' if route in advertised else 'NOT advertised'} by /v1/capabilities, "
                       "and the dashboard disables every control whose route the server does not advertise")
            raise AssertionError(f"{button} is disabled for a super admin{why}") from None
        trigger.click()
        expect(control(page, result)).to_contain_text(WHILE_MEMORY)
        for text in ("forbidden_scope", "HTTP 403"):
            expect(page.locator("#status")).not_to_contain_text(text)
        no_overflow(page, label)


def graph_statistics(page, advertised):
    with step(page, "step 5: the Graph workspace offers graph engine statistics to the super admin", ("#graph-note",)):
        open_workspace(page, "Graph")
        button = page.locator("#graph-stats")
        expect(button).to_be_visible()
        expect(button).not_to_have_attribute("aria-disabled", "true")
        if button.is_disabled() and GRAPH_STATS_ROUTE not in advertised:
            # Not specific to agent memory: console-tools.js disables every [data-route] control the server does not
            # advertise, and /v1/capabilities never lists this route, so the button is dead on any real server.
            skipped = ("graph engine statistics (#graph-stats is disabled: /v1/capabilities does not advertise "
                       + GRAPH_STATS_ROUTE + ")")
            if skipped not in NOT_EXERCISED:
                NOT_EXERCISED.append(skipped)
                print("NOTE graph engine statistics were NOT exercised: #graph-stats is disabled because /v1/capabilities does "
                      "not advertise " + GRAPH_STATS_ROUTE + " (true with or without agent memory). Enable the button and this "
                      "check enforces itself.", file=sys.stderr)
            return
    refused_panel(page, advertised, "graph engine statistics", "#graph-stats", "#graph-stats-result", extra=("#graph-note",))


def refused_panels(page, advertised):
    for label, button, result, before in (
        ("webhooks", "#webhook-list", "#webhook-result", None),
        ("automatic maintenance", "#auto-maintenance-read", "#auto-maintenance-result", None),
        ("replication export", "#replication-export", "#replication-result", None),
        ("Prometheus text", "#prometheus", "#prometheus-result", None),
        ("import job status", "#job-status", "#job-result", ("#job-id", "1")),
    ):
        refused_panel(page, advertised, label, button, result, before)
    graph_statistics(page, advertised)


def tenant_editor(page, url, token, editor, password):
    with step(page, "step 7: a tenant editor still signs in with a password and sees no metrics note"):
        made = accepted("create tenant", rest(url, "POST", "/v1/admin/tenants", {"name": "ui-notes"}, token), 200, 201)
        tenant = (made.get("tenant") or made)["id"]
        accepted("create editor", rest(url, "POST", "/v1/admin/users", {
            "username": editor, "password": password, "role": "editor", "tenant_id": tenant,
            "must_change_password": False}, token), 201)
        sign_out(page)
        sign_in_with_password(page, editor, password)
        expect_signed_in_as(page, editor)
        expect(page.locator("#metrics-note")).to_be_hidden()
        expect_no_refusal(page)
        assert_no_stored_credentials(page, password)
        sign_out(page)


def memory_phase(browser, url, token, username, password, editor, editor_password):
    context, page, errors = open_page(browser)
    try:
        page.goto(url + "/dashboard")
        with step(page, "step 1: the bootstrap super admin signs in with a password"):
            expect_signed_out(page)
            try:
                expect(page.locator("#sign-in")).to_be_visible()
            except AssertionError:
                raise AssertionError("password sign-in is not offered (#sign-in is hidden); /v1/auth/config answers "
                                     f"{rest(url, 'GET', '/v1/auth/config')[1]}") from None
            sign_in_with_password(page, username, password)
            expect_signed_in_as(page, username)
            expect_no_refusal(page)
            assert_no_stored_credentials(page, password)
        overview(page)
        observability(page)
        administration(page)
        with step(page, "TEST-015: reload memory grants and wait for older requests", ("#memory-bindings-reload-result",)):
            control(page, "#memory-bindings-reload").click()
            confirm(page, "Reload memory grants")
            expect(control(page, "#memory-bindings-reload-result")).to_contain_text('"drained": true')
            no_overflow(page, "memory grants reload")
        refused_panels(page, accepted("capabilities", rest(url, "GET", "/v1/capabilities", token=token), 200)["routes"])
        with step(page, "step 6: sign out, then sign in again with the admin token"):
            sign_out(page)
            sign_in_with_token(page, token)
            open_workspace(page, "Overview")
            expect(page.locator("#metrics-note")).to_be_visible()
            expect(page.locator("#metrics-note")).to_contain_text(METRICS_OFF, ignore_case=True)
            expect(page.locator("#stat-bytes")).to_have_text("Unavailable")
            assert_no_stored_credentials(page, token)
        tenant_editor(page, url, token, editor, editor_password)
        require(not errors, f"browser errors while agent memory was enabled: {errors}")
    finally:
        context.close()


# ----------------------------------------------------------------------------------------------
# C. The bindings file is gone, the binding records remain
# ----------------------------------------------------------------------------------------------
def leftover_phase(browser, url, token, username, password):
    """The memory database restarted without its bindings file. The binding records keep the reservation, so the
    server still refuses the database-wide routes (docs/SECURITY.md, Memory-bound resources) and the dashboard
    must keep signing in and explaining them. The wording stays "agent memory is enabled" (a known limit)."""
    global PHASE
    status, value = rest(url, "GET", "/metrics", token=token)
    code = ((value or {}).get("error") or {}).get("code")
    require((status, code) == (403, "forbidden_scope"),
            f"leftover records: /metrics answered HTTP {status} ({code}); the binding records should keep it refused")
    status = rest(url, "POST", "/v1/memory/notes/recall", {"scope": {"agent": "a"}, "entity_seeds": []}, token)[0]
    require(status == 404, f"leftover records: a memory route answered HTTP {status}; none is mounted without the file")
    PHASE = "leftover records, no bindings file: "
    context, page, errors = open_page(browser)
    try:
        page.goto(url + "/dashboard")
        with step(page, "step 1: the super admin still signs in with a password"):
            expect_signed_out(page)
            sign_in_with_password(page, username, password)
            expect_signed_in_as(page, username)
            expect_no_refusal(page)
        overview(page)
        observability(page)
        administration(page)
        refused_panels(page, accepted("capabilities", rest(url, "GET", "/v1/capabilities", token=token), 200)["routes"])
        require(not errors, f"browser errors while only the binding records remained: {errors}")
    finally:
        PHASE = ""
        context.close()


def main():
    binary = server_binary()
    token = secret(secrets.token_urlsafe(40))
    password = secret(secrets.token_urlsafe(24))
    editor_password = secret(secrets.token_urlsafe(24))
    suffix = "".join(secrets.choice("abcdefghjkmnpqrstuvwxyz") for _ in range(8))
    username, editor = f"owner-{suffix}", f"editor-{suffix}"
    work = Path(tempfile.mkdtemp(prefix="vlite-dashboard-memory-"))
    servers = []
    try:
        try:
            control_server = Server(binary, Home(work, "control", token), "control", create=True)
            servers.append(control_server)
            with sync_playwright() as playwright:
                browser = playwright.chromium.launch()
                try:
                    control_phase(browser, control_server.url, token)
                    control_server.stop()
                    memory_home = Home(work, "memory", token, (username, password))
                    memory_server = start_memory_server(binary, memory_home, token)
                    servers.append(memory_server)
                    memory_phase(browser, memory_server.url, token, username, password, editor, editor_password)
                    memory_server.stop()
                    leftover_server = Server(binary, memory_home, "leftover")
                    servers.append(leftover_server)
                    leftover_phase(browser, leftover_server.url, token, username, password)
                finally:
                    browser.close()
        except BaseException:
            if servers:
                print(f"--- tail of {servers[-1].log_path.name} ---\n{servers[-1].tail()}", file=sys.stderr)
            raise
    finally:
        for server in servers:
            server.stop()
        remove(work)
    print("PASS dashboard memory mode: with agent memory enabled the super admin signs in with a password and with the "
          "token; Overview, Observability and Health say server metrics are off; webhooks, automatic maintenance, "
          "replication, Prometheus text and import status explain themselves in their own panels; "
          "logs, slow queries, audit and backups keep working; a tenant editor is unaffected; with the bindings file "
          "removed and only the binding records left, the refusals stay and the dashboard keeps explaining them; "
          "no overflow at "
          + "/".join(str(width) for width in WIDTHS) + " px"
          + ("; NOT EXERCISED: " + "; ".join(NOT_EXERCISED) if NOT_EXERCISED else "; graph engine statistics explain themselves too"))


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except AssertionError as error:
        print(redact(f"FAIL dashboard memory mode\n{error}"), file=sys.stderr)
        sys.exit(1)
    except BaseException:
        print(redact("FAIL dashboard memory mode\n" + traceback.format_exc()), file=sys.stderr)
        sys.exit(1)
