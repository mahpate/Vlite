"""Capture real v0.4.1 dashboard workflows as finite-loop animated PNGs.

Run from Linux/WSL with Docker, Playwright Chromium and Pillow:
python scripts/capture_dashboard_tours.py <output-directory>
Uses an isolated tmpfs database and removes only its own container on exit.
Requires playwright (tests/requirements-dashboard.txt) and Pillow==11.3.0.
"""

import base64
from io import BytesIO
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import time
import urllib.request
import uuid

from PIL import Image
from playwright.sync_api import sync_playwright, expect

IMAGE = "ghcr.io/mahpate/vlite-api:v0.4.1"


def docker(*args, **kwargs):
    return subprocess.check_output(["docker", *args], text=True, **kwargs).strip()


def capture(output):
    output.mkdir(parents=True, exist_ok=True)
    token = secrets.token_urlsafe(32)
    name = "vlite-site-tour-" + uuid.uuid4().hex[:10]
    env = {**os.environ, "VLITE_AUTH_TOKEN": token}
    docker("run", "-d", "--name", name, "--mount", "type=tmpfs,target=/data,tmpfs-mode=1777",
           "-e", "VLITE_AUTH_TOKEN", "-p", "127.0.0.1::8080", "--entrypoint", "vlite-server",
           IMAGE, "/data/demo.vlite", "--create-if-missing", "3", "--create-format", "4",
           "--bind", "0.0.0.0:8080", "--allow-remote", env=env)
    try:
        port = docker("port", name, "8080/tcp").rsplit(":", 1)[1]
        base = "http://127.0.0.1:" + port

        def api(path, body=None, method=None):
            data = json.dumps(body).encode() if body is not None else None
            request = urllib.request.Request(base + path, data=data, method=method,
                                             headers={"Authorization": "Bearer " + token,
                                                      "Content-Type": "application/json"})
            with urllib.request.urlopen(request, timeout=10) as response:
                return response.read()

        deadline = time.monotonic() + 30
        while True:
            try:
                api("/health")
                break
            except (OSError, urllib.error.URLError):
                if time.monotonic() > deadline:
                    raise
                time.sleep(.25)
        for i, title in enumerate(("Getting started with VLite", "A retrieval workflow",
                                   "Typed metadata", "Inspecting server metrics", "Backing up data"), 101):
            api(f"/v1/vectors/{i}", {"vector": [1 if i == 101 else .2, (i - 101) / 5, .1],
                "payload_base64": base64.b64encode(title.encode()).decode(),
                "metadata": {"title": {"type": "string", "value": title},
                             "category": {"type": "string", "value": "demo"}}}, "PUT")

        with sync_playwright() as p:
            browser = p.chromium.launch()
            page = browser.new_page(viewport={"width": 1440, "height": 1100},
                                    device_scale_factor=1, reduced_motion="reduce")
            page.goto(base + "/dashboard")
            page.locator("#token").fill(token)
            page.locator("#connection button[type=submit]").click()
            expect(page.locator("#connection-badge")).to_have_text("Connected")
            page.get_by_role("button", name="Dismiss tour", exact=True).click()
            nav = page.get_by_role("navigation", name="Dashboard navigation")

            def frame():
                page.evaluate("window.scrollTo(0, 0)")
                return Image.open(BytesIO(page.screenshot(
                    mask=[page.locator("#token")], mask_color="#e4e6ef",
                    animations="disabled"))).convert("RGB")

            def save(name, frames):
                frames[0].save(output / f"{name}-poster.png", optimize=True)
                frames[0].save(output / f"{name}.png", save_all=True, append_images=frames[1:],
                               duration=2200, loop=1, optimize=True)
                with Image.open(output / f"{name}.png") as image:
                    assert image.n_frames == len(frames) and image.info["loop"] == 1
                assert (output / f"{name}.png").stat().st_size < 3_000_000

            nav.get_by_role("link", name="Collections", exact=True).click()
            page.locator("#browse-form button[type=submit]").click()
            expect(page.locator("#vectors tbody tr")).to_have_count(5)
            page.locator("#vectors tbody button").first.click()
            expect(page.locator("#record-inspector-payload")).to_contain_text("Getting started")
            records = [frame()]
            for tab in ("Payload", "Metadata", "Vector", "Raw JSON"):
                page.get_by_role("tablist", name="Selected record views").get_by_role("tab", name=tab, exact=True).click()
                page.locator("#record-inspector").scroll_into_view_if_needed()
                records.append(Image.open(BytesIO(page.screenshot(animations="disabled"))).convert("RGB"))
            page.get_by_role("tab", name="Write", exact=True).click()
            records.append(frame())
            page.get_by_role("tab", name="Index", exact=True).click()
            records.append(frame())
            page.get_by_role("tab", name="Settings", exact=True).click()
            records.append(frame())
            save("records-tour", records)

            nav.get_by_role("link", name="Search", exact=True).click()
            page.locator("#query").fill("[0.9, 0.1, 0]")
            page.locator("#search-form button[type=submit]").click()
            expect(page.locator("#hits tbody tr")).to_have_count(5)
            search = [frame()]
            page.locator("#hits tbody button").first.click()
            page.locator("#record-inspector").scroll_into_view_if_needed()
            search.append(Image.open(BytesIO(page.screenshot(animations="disabled"))).convert("RGB"))
            page.locator("#mode").select_option("approximate")
            page.locator("#search-form button[type=submit]").click()
            expect(page.locator("#search-summary")).to_contain_text("approximate")
            search.append(frame())
            save("search-tour", search)

            operations = []
            for workspace in ("Overview", "Observability", "Administration", "Developer Console"):
                nav.get_by_role("link", name=workspace, exact=True).click()
                if workspace == "Observability":
                    page.locator("#obs-refresh").click()
                    expect(page.locator("#obs-metrics-status")).to_contain_text("Live metrics")
                operations.append(frame())
            save("operations-tour", operations)
            browser.close()
        print("Captured three real dashboard tours from", IMAGE)
    finally:
        docker("rm", "-f", name)


if __name__ == "__main__":
    capture(Path(sys.argv[1]).resolve())
