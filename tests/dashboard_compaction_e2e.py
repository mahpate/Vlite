"""Drive the dashboard File compaction card against a real vlite-server in Chromium.

Run after building the server with the test hooks:
    cargo build --features "server,compaction-test-hooks" --bin vlite-server
    python tests/dashboard_compaction_e2e.py [path-to-vlite-server]

The server runs with VLITE_COMPACTION_TEST_PROFILE=1 (lifts the size floors and the
filesystem gate so a small temporary database qualifies) and, for the cancel scenario,
the very_slow_build fault point. Nothing here touches any database but a temporary one.
"""

import json
import os
import subprocess
import sys
import tempfile
import threading
import urllib.request
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

from admin_e2e import sign_in
from dashboard_e2e import confirm, control, sign_in_with_token

ROOT = Path(__file__).resolve().parents[1]
TOKEN = "dashboard_compaction_admin_0123456789_ABCDEFGH"
READ = "dashboard_compaction_read_0123456789_ABCDEFGHI"
USER = "operator"
PASSWORD = "correct horse battery staple 42"
PAYLOAD = "QUFB" * 64
EXE = ".exe" if os.name == "nt" else ""


class Server:
    def __init__(self, binary, workdir, faults=""):
        workdir.mkdir(parents=True, exist_ok=True)
        password = workdir / "password.txt"
        password.write_text(PASSWORD, encoding="utf-8")
        env = {key: value for key, value in os.environ.items() if not key.startswith("VLITE_")}
        env.update({
            "VLITE_AUTH_TOKEN": TOKEN,
            "VLITE_READ_ONLY_TOKEN": READ,
            "VLITE_COMPACTION_TEST_PROFILE": "1",
            "VLITE_COMPACTION_FAULT": faults,
            "VLITE_ADMIN_DIR": str(workdir / "admin"),
            "VLITE_ADMIN_USERNAME": USER,
            "VLITE_ADMIN_PASSWORD_FILE": str(password),
        })
        self.process = subprocess.Popen(
            [str(binary), str(workdir / "live.vlite"), "--create-if-missing", "2", "--bind", "127.0.0.1:0"],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        line = [None]
        reader = threading.Thread(target=lambda: line.__setitem__(0, self.process.stdout.readline()), daemon=True)
        reader.start()
        reader.join(120)
        if not line[0] or not line[0].startswith("LISTENING http://"):
            self.stop()
            raise RuntimeError(f"server did not start: {line[0]!r}")
        self.base = line[0].strip().removeprefix("LISTENING ")

    def call(self, method, path, body=None, token=TOKEN):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data=data, method=method, headers={
            "Authorization": f"Bearer {token}", "Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=60) as response:
            raw = response.read()
            return json.loads(raw) if raw else None

    def churn(self, rounds):
        """Overwrites the same records repeatedly so the file holds reclaimable space."""
        for round_number in range(rounds):
            for vector_id in range(1, 11):
                self.call("PUT", f"/v1/vectors/{vector_id}",
                          {"vector": [float(vector_id), float(round_number)], "payload_base64": PAYLOAD})

    def stop(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(30)


def open_card(page, server, token=TOKEN):
    page.goto(server.base + "/dashboard")
    sign_in_with_token(page, token)
    expect(control(page, "#admin-compaction")).to_be_visible()


def status(page):
    return page.locator("#compaction-status")


def no_overflow(page):
    for width, height in ((1280, 900), (768, 1000), (390, 844)):
        page.set_viewport_size({"width": width, "height": height})
        control(page, "#admin-compaction")
        overflow = page.evaluate("""() => {
            const card = document.querySelector('#admin-compaction');
            return { page: document.documentElement.scrollWidth - document.documentElement.clientWidth,
                     card: card.scrollWidth - card.clientWidth };
        }""")
        assert overflow["page"] <= 0 and overflow["card"] <= 0, (width, overflow)
        for selector in ("#compaction-start", "#compaction-enabled", "#compaction-ack-downtime"):
            box = page.locator(selector).bounding_box()
            assert box and box["width"] > 0, (width, selector)
        target = page.locator("#compaction-start").bounding_box()
        assert target["height"] >= 44, (width, target)
    page.set_viewport_size({"width": 1280, "height": 900})


def compact_now(page, server):
    """Acknowledgements gate the run; the confirmed run finishes with before/after sizes."""
    before = server.call("GET", "/v1/admin/compaction")["file"]["bytes"]
    expect(status(page)).to_contain_text("Ready")
    control(page, "#compaction-start").click()
    expect(page.locator("#compaction-ack-error")).to_be_visible()
    expect(page.locator("#compaction-ack-downtime")).to_be_focused()
    page.locator("#compaction-ack-downtime").check()
    page.locator("#compaction-ack-history").check()
    page.locator("#compaction-start").click()
    confirm(page, "Compact the database file now?")
    expect(status(page)).to_contain_text("Finished: the server now uses the compacted file.", timeout=60_000)
    facts = page.locator("#compaction-facts")
    expect(facts).to_contain_text("Last size change")
    expect(facts).to_contain_text("saved")
    expect(facts).to_contain_text("Selected compacted file")
    after = server.call("GET", "/v1/admin/compaction")
    assert after["last_job"]["outcome"] == "succeeded", after["last_job"]
    assert after["file"]["bytes"] < before, (before, after["file"])
    assert server.call("GET", "/v1/vectors/3")["vector"] == [3.0, 19.0]
    # The acknowledgements reset after a run, so the next run needs them again.
    expect(page.locator("#compaction-ack-downtime")).not_to_be_checked()


def settings(page, server):
    """Invalid values show inline errors and send nothing; a valid schedule saves and persists."""
    version = server.call("GET", "/v1/admin/compaction")["settings_version"]
    page.locator("#compaction-enabled").check()
    page.locator("#compaction-start-utc").fill("25:00")
    page.locator("#compaction-window").fill("4")
    page.locator("#compaction-save").click()
    expect(page.locator("#compaction-start-utc-error")).to_have_text("Enter a 24-hour UTC time such as 02:00.")
    expect(page.locator("#compaction-window-error")).to_be_visible()
    expect(page.locator("#compaction-history-policy-error")).to_have_text("Automatic compaction needs this consent.")
    expect(page.locator("#compaction-start-utc")).to_have_attribute("aria-invalid", "true")
    expect(page.locator("#compaction-start-utc")).to_be_focused()
    assert server.call("GET", "/v1/admin/compaction")["settings_version"] == version
    for selector, value in (("#compaction-start-utc", "03:30"), ("#compaction-window", "60"),
                            ("#compaction-max-run", "900"), ("#compaction-min-file", "0"),
                            ("#compaction-min-growth", "1"), ("#compaction-growth-percent", "25"),
                            ("#compaction-cooldown", "48"), ("#compaction-max-source", "512")):
        page.locator(selector).fill(value)
    page.locator("#compaction-history-policy").check()
    page.locator("#compaction-save").click()
    confirm(page, "Turn on automatic compaction daily at 03:30 UTC")
    expect(page.locator("#compaction-result")).to_contain_text("Saved. Automatic compaction is on")
    saved = server.call("GET", "/v1/admin/compaction")["settings"]
    assert saved["enabled"] is True and saved["daily_start_utc"] == "03:30", saved
    assert saved["window_minutes"] == 60 and saved["max_run_seconds"] == 900, saved
    assert saved["history_policy"] == "resnapshot_allowed" and saved["cooldown_hours"] == 48, saved
    assert saved["max_source_bytes"] == 512 * 1024 * 1024, saved
    page.reload()
    sign_in_with_token(page, TOKEN) if page.locator("#sign-in-gate").is_visible() else None
    control(page, "#admin-compaction")
    expect(page.locator("#compaction-enabled")).to_be_checked()
    expect(page.locator("#compaction-start-utc")).to_have_value("03:30")
    # Turning it back off needs no consent and still saves.
    page.locator("#compaction-enabled").uncheck()
    page.locator("#compaction-save").click()
    confirm(page, "Save these compaction settings with automatic compaction off?")
    expect(page.locator("#compaction-result")).to_contain_text("Automatic compaction is off")
    assert server.call("GET", "/v1/admin/compaction")["settings"]["enabled"] is False


def read_only(browser, server):
    page = browser.new_page()
    open_card(page, server, READ)
    for selector in ("#compaction-start", "#compaction-save", "#compaction-enabled"):
        expect(page.locator(selector)).to_be_disabled()
    expect(page.locator('[data-admin-availability="compaction"]')).to_contain_text("Needs a super admin")
    page.close()


def password_session(browser, server):
    """A cookie session (CSRF header path) can start a run that the server finishes."""
    page = browser.new_page()
    page.goto(server.base + "/dashboard")
    sign_in(page, USER, PASSWORD)
    expect(page.locator("#status")).to_have_text(f"Signed in as {USER}; overview refreshed")
    control(page, "#admin-compaction")
    expect(status(page)).to_contain_text("Ready")
    page.locator("#compaction-ack-downtime").check()
    page.locator("#compaction-ack-history").check()
    page.locator("#compaction-start").click()
    confirm(page, "Compact the database file now?")
    expect(status(page)).to_contain_text("Finished", timeout=60_000)
    page.close()


def cancel(page, server):
    """A slow build stays cancellable; cancelling keeps the original file and data."""
    before = server.call("GET", "/v1/admin/compaction")["file"]["bytes"]
    page.locator("#compaction-ack-downtime").check()
    page.locator("#compaction-ack-history").check()
    page.locator("#compaction-start").click()
    confirm(page, "Compact the database file now?")
    expect(status(page)).to_contain_text("Writing the compacted file", timeout=30_000)
    expect(page.locator("#compaction-cancel")).to_be_enabled()
    expect(page.locator("#compaction-save")).to_be_disabled()
    page.locator("#compaction-cancel").click()
    confirm(page, "Cancel the running compaction?")
    expect(status(page)).to_contain_text("Cancelled: the server kept using the original file.", timeout=60_000)
    expect(page.locator("#compaction-cancel")).to_be_disabled()
    after = server.call("GET", "/v1/admin/compaction")
    assert after["last_job"]["outcome"] == "cancelled", after["last_job"]
    assert after["file"]["bytes"] == before, (before, after["file"])
    assert server.call("GET", "/v1/vectors/3")["vector"] == [3.0, 9.0]


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else
                  os.environ.get("VLITE_SERVER_BIN", ROOT / "target" / "debug" / f"vlite-server{EXE}"))
    if not binary.is_file():
        raise SystemExit(f"build vlite-server with --features server,compaction-test-hooks first: {binary}")
    with tempfile.TemporaryDirectory(dir=ROOT / "target") as temp, sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        errors = []
        server = Server(binary, Path(temp) / "plain")
        try:
            server.churn(20)
            page = browser.new_page()
            page.on("pageerror", lambda error: errors.append(str(error)))
            open_card(page, server)
            no_overflow(page)
            compact_now(page, server)
            settings(page, server)
            read_only(browser, server)
            server.churn(20)
            password_session(browser, server)
            page.close()
        finally:
            server.stop()
        server = Server(binary, Path(temp) / "slow", "very_slow_build")
        try:
            server.churn(10)
            page = browser.new_page()
            page.on("pageerror", lambda error: errors.append(str(error)))
            open_card(page, server)
            cancel(page, server)
            page.close()
        finally:
            server.stop()
        browser.close()
        assert not errors, errors
    print("dashboard compaction e2e passed")


if __name__ == "__main__":
    main()
