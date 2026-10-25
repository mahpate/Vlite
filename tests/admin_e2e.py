"""Exercise production admin UI against deterministic API responses in Chromium.

Run on a hosted runner: python tests/admin_e2e.py.
No Docker, real credentials, server, or database is used.
"""

from playwright.sync_api import expect, sync_playwright

from dashboard_e2e import FakeApi, confirm, control, mocked_page


TENANT = "a" * 32
ADMIN_FEATURES = ("tenants", "native_tls", "tls_reload", "credential_rate_limits",
                  "credential_vector_quotas")
EXPORT = "/v1/admin/replication/export"


class AdminApi(FakeApi):
    def __init__(self, role="admin"):
        super().__init__(role=role)
        self.caps["features"].update({name: True for name in ADMIN_FEATURES})
        self.caps["routes"] = [EXPORT]
        self.tenants = []
        self.nonempty = False
        self.tls_error = False

    def api(self, method, path, query, body, headers):
        if path == "/v1/admin/tenants":
            if method == "POST":
                tenant = {"id": TENANT, **body}
                self.tenants.append(tenant)
                return 201, {"tenant": tenant}, {}
            return 200, {"items": self.tenants}, {}
        if path == f"/v1/admin/tenants/{TENANT}" and method == "DELETE":
            if self.nonempty:
                return self.error(409, "tenant_not_empty")
            self.tenants = []
            return 200, {"deleted": TENANT}, {}
        if path == "/v1/admin/tls/reload":
            return self.error(422, "tls_reload_failed") if self.tls_error else (200, {"reloaded": True}, {})
        if path == EXPORT:
            return 200, {"segment_base64": "AQID", "lineage": "demo",
                         "base_sequence": "0", "last_sequence": "5", "head_sequence": "9"}, {}
        return super().api(method, path, query, body, headers)


def calls(fake, path, method):
    return [item for item in fake.log if item["path"] == path and item["method"] == method]


def exercise(browser):
    fake = AdminApi()
    context, page, errors = mocked_page(browser, fake)
    assert not calls(fake, "/v1/admin/tenants", "GET"), "Admin actions must not run on connection"
    control(page, "#tenant-list").click()
    expect(control(page, "#tenant-result")).to_contain_text('"count": 0')
    control(page, "#tenant-name").fill("Support")
    control(page, "#tenant-max-vectors").fill("100")
    control(page, "#tenant-create").click()
    confirm(page, "Create tenant", accept=False)
    assert not calls(fake, "/v1/admin/tenants", "POST")
    control(page, "#tenant-create").click()
    confirm(page, "aggregate limit of 100")
    expect(control(page, "#tenant-result")).to_contain_text(TENANT)
    assert calls(fake, "/v1/admin/tenants", "POST")[-1]["body"] == {
        "name": "Support", "max_vectors": 100}
    fake.nonempty = True
    control(page, "#tenant-delete").click()
    confirm(page, "cannot undo")
    expect(control(page, "#tenant-result")).to_contain_text("still owns collections")
    fake.nonempty = False
    control(page, "#tenant-delete").click()
    confirm(page, "cannot undo")
    expect(control(page, "#tenant-result")).to_contain_text('"deleted"')
    control(page, "#tls-reload").click()
    confirm(page, "Nothing is uploaded", accept=False)
    assert not calls(fake, "/v1/admin/tls/reload", "POST")
    control(page, "#tls-reload").click()
    confirm(page, "Existing connections")
    expect(control(page, "#tls-result")).to_contain_text('"reloaded": true')
    fake.tls_error = True
    control(page, "#tls-reload").click()
    confirm(page, "Existing connections")
    expect(control(page, "#tls-result")).to_contain_text("previous certificates remain active")
    control(page, "#replication-max-bytes").fill("1024")
    control(page, "#replication-export").click()
    expect(control(page, "#replication-result")).to_contain_text('"more_available": true')
    expect(control(page, "#replication-after")).to_have_value("5")
    with page.expect_download() as download:
        control(page, "#replication-download").click()
    assert download.value.suggested_filename == "vlite-replication-demo-0-5.json"
    assert calls(fake, EXPORT, "GET")[-1]["query"] == "max_bytes=1024"
    for width in (1440, 900, 390, 320):
        page.set_viewport_size({"width": width, "height": 1000})
        for section in ("tenants", "transport-security", "replication", "admin-backups",
                        "admin-auto-maintenance", "admin-backup-schedule"):
            page.evaluate("id => window.VLiteNavigation.reveal(id)", section)
            assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), (width, section)
    page.evaluate("window.VLiteNavigation.reveal('database')")
    control(page, "#disconnect").click()
    expect(control(page, "#tenant-result")).to_have_text("")
    expect(control(page, "#tls-result")).to_have_text("")
    expect(control(page, "#replication-result")).to_have_text("")
    expect(control(page, "#replication-download")).to_be_disabled()
    assert not errors, errors
    context.close()

    for role in ("read_only", "scoped"):
        fake = AdminApi(role)
        if role == "scoped":
            fake.caps["scope"] = {"collections": ["docs"], "operations": ["read", "search"],
                                 "expires_unix_ms": 9999999999999}
        context, page, errors = mocked_page(browser, fake)
        for name in ("tenant-list", "tenant-create", "tenant-delete", "tls-reload"):
            expect(control(page, "#" + name)).to_be_disabled()
        expect(control(page, "#tenant-result")).to_contain_text("bootstrap admin")
        assert not errors, errors
        context.close()
    fake = AdminApi()
    fake.caps["features"]["tenants"] = False
    fake.caps["features"]["tls_reload"] = False
    context, page, errors = mocked_page(browser, fake)
    expect(control(page, "#tenant-list")).to_be_disabled()
    expect(control(page, "#tls-reload")).to_be_disabled()
    expect(control(page, "#tenant-result")).to_contain_text("VLITE_ADMIN_DIR")
    expect(control(page, "#tls-result")).to_contain_text("file-backed TLS")
    assert not errors, errors
    context.close()


if __name__ == "__main__":
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        try:
            exercise(browser)
        finally:
            browser.close()
    print("PASS browser admin flows: confirmations/cancel, tenant CRUD/errors, TLS reload/errors, "
          "replication download/paging, disconnect clearing, role/configuration gates and mobile navigation.")
